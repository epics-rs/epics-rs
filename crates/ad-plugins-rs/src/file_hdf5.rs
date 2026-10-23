use std::path::{Path, PathBuf};
use std::sync::Arc;

use ad_core_rs::attributes::{
    NDAttrDataType, NDAttrSource, NDAttrValue, NDAttribute, NDAttributeList,
};
use ad_core_rs::codec::{Codec, CodecName};
use ad_core_rs::color::NDColorMode;
use ad_core_rs::error::{ADError, ADResult};
use ad_core_rs::finalize::Finalize;
use ad_core_rs::ndarray::{NDArray, NDDataBuffer, NDDataType, NDDataView};
use ad_core_rs::ndarray_pool::NDArrayPool;
use ad_core_rs::plugin::file_base::{FileAttributes, NDFileOpenMode, NDFileWriter};
use ad_core_rs::plugin::file_controller::{
    FileControl, FilePluginController, SettingsStore, WriterSettings,
};
use ad_core_rs::plugin::runtime::{
    NDPluginProcess, ParamChangeResult, ParamChangeValue, ParamUpdate, PluginAttributes,
    PluginParamSnapshot, PluginPool, PortPost, ProcessResult, WriteOutcome, WritePort,
    WriteVerdict,
};
use asyn_rs::port::InterfaceMask;
use asyn_rs::port::PortDriverBase;
use asyn_rs::trace::TraceMask;
use parking_lot::Mutex;

use rust_hdf5::H5File;
use rust_hdf5::format::messages::datatype::DatatypeMessage;
use rust_hdf5::format::messages::filter::{
    FILTER_BLOSC, FILTER_BSHUF, FILTER_JPEG, FILTER_SZIP, Filter, FilterPipeline,
};
use rust_hdf5::swmr::SwmrFileWriter;
use rust_hdf5::types::H5Type;

use crate::hdf5_layout::{Hdf5Layout, LayoutWhen, NdAttrElementAttr};

/// C ADCore compression type enum values (matching NDFileHDF5.h).
const COMPRESS_NONE: i32 = 0;
const COMPRESS_NBIT: i32 = 1;
const COMPRESS_SZIP: i32 = 2;
const COMPRESS_ZLIB: i32 = 3;
const COMPRESS_BLOSC: i32 = 4;
const COMPRESS_BSHUF: i32 = 5;
const COMPRESS_LZ4: i32 = 6;
const COMPRESS_JPEG: i32 = 7;
/// C's enum has LZ4HDF5 = 7 and JPEG = 8 (NDFileHDF5.cpp:36-44), but the
/// Compression menu writes 7 for "JPEG" (NDFileHDF5.template:604-605), so in C
/// that menu entry selects LZ4HDF5. This port keeps the menu's meaning; lz4hdf5
/// is reached only through a frame's codec (`Hdf5Settings::codec_override`),
/// and the 8 it posts to Compression_RBV has no menu state, as C's JPEG = 8.
const COMPRESS_LZ4HDF5: i32 = 8;

/// SZIP nearest-neighbor coding option mask (`H5_SZIP_NN_OPTION_MASK`,
/// H5Zpublic.h); C passes this to `H5Pset_szip` (NDFileHDF5.cpp:3372).
const SZIP_NN_OPTION_MASK: u32 = 32;

/// Maximum number of extra dimensions (C `MAXEXTRADIMS`).
const MAX_EXTRA_DIMS: usize = 10;

/// Built-in NeXus layout used when no user layout XML is configured, mirroring
/// C `LayoutXML::DEFAULT_LAYOUT` (NDFileHDF5LayoutXML.cpp:43-70) which C loads
/// via `layout.load_xml()` for an empty `HDF5_layoutFilename` (NDFileHDF5.cpp:
/// 3896-3906). It places the detector image at `/entry/instrument/detector/data`
/// inside a full NXentry/NXinstrument/NXdetector/NXcollection/NXdata tree, with
/// an `/entry/data/data` hardlink to the detector dataset.
const DEFAULT_LAYOUT_XML: &str = r#"
<hdf5_layout>
<group name="entry">
  <attribute name="NX_class" source="constant" value="NXentry" type="string"></attribute>
  <group name="instrument">
    <attribute name="NX_class" source="constant" value="NXinstrument" type="string"></attribute>
    <group name="detector">
      <attribute name="NX_class" source="constant" value="NXdetector" type="string"></attribute>
      <dataset name="data" source="detector" det_default="true">
        <attribute name="signal" source="constant" value="1" type="int"></attribute>
      </dataset>
      <group name="NDAttributes">
        <attribute name="NX_class" source="constant" value="NXcollection" type="string"></attribute>
        <dataset name="ColorMode" source="ndattribute" ndattribute="ColorMode"></dataset>
      </group>
    </group>
    <group name="NDAttributes" ndattr_default="true">
      <attribute name="NX_class" source="constant" value="NXcollection" type="string"></attribute>
    </group>
    <group name="performance">
      <dataset name="timestamp"></dataset>
    </group>
  </group>
  <group name="data">
    <attribute name="NX_class" source="constant" value="NXdata" type="string"></attribute>
    <hardlink name="data" target="/entry/instrument/detector/data"></hardlink>
  </group>
</group>
</hdf5_layout>
"#;

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
    /// `0` means auto (C param default), resolved by `attribute_chunking`.
    ndattr_chunk: usize,
}

impl Default for ChunkConfig {
    fn default() -> Self {
        Self {
            auto: true,
            n_row_chunks: 0,
            n_col_chunks: 0,
            n_frames_chunks: 1,
            ndattr_chunk: 0,
        }
    }
}

/// One extra-dimension entry (C `HDF5_extraDimSizeN` / `HDF5_extraDimNameN` /
/// `HDF5_extraDimChunkN`).
#[derive(Clone, Default)]
struct ExtraDim {
    size: usize,
    name: String,
    /// As written: C's `verifyChunking` reads it unclamped.
    chunk: i32,
}

/// State for a single open attribute time-series dataset (one per NDAttribute).
/// Mirrors C++ `NDFileHDF5AttributeDataset`: a 1-D extensible dataset holding
/// one numeric (or string) value per frame.
struct AttributeDataset {
    name: String,
    /// The three remaining self-describing strings C writes as HDF5 attributes
    /// on each attribute dataset (NDFileHDF5.cpp:2715, 2785-2788): description,
    /// source, and the source-type string. Captured at create time because the
    /// source NDArray is gone by flush.
    description: String,
    source: String,
    source_type: String,
    data_type: NDAttrDataType,
    /// C `NDFileHDF5AttributeDataset::isUndefined_`
    /// (`typeAsHdf`'s `default:` arm, NDFileHDF5AttributeDataset.cpp:366-370).
    /// Fixed for the dataset's whole life: an attribute carrying no value when
    /// the file opens gets a float32 dataset whose `epicsNAN` fill no frame
    /// ever overwrites, because every element write is gated on
    /// `!isUndefined_` (`:160`).
    undefined: bool,
    /// Parent group (no leading `/`; empty is the file root) and dataset
    /// name.
    group: String,
    dset_name: String,
    /// C `whenToSave_`: the layout dataset's `when`, else `OnFrame`.
    when: LayoutWhen,
    shape: AttrShape,
    /// The frame offsets, outermost first, a positioned write puts this
    /// dataset's element at (C `writeAttributeDataset(…, offsets, …, indexed)`,
    /// NDFileHDF5AttributeDataset.cpp:176-189): one for an index dataset,
    /// every axis of a dimAttDatasets grid, none for a scalar or a plain
    /// series.
    offset_axes: Vec<usize>,
    /// C `dims_`: 1 on each axis until an element lands further out.
    extent: Vec<usize>,
    sink: AttrSink,
    /// Elements written through `sink` (C `nextRecord_`).
    count: usize,
}

/// C `NDFileHDF5AttributeDataset` geometry: `configureDims` or, under
/// `HDF5_dimAttDatasets`, `configureDimsFromDataset`
/// (NDFileHDF5AttributeDataset.cpp:231-311).
#[derive(Clone, Debug, PartialEq)]
enum AttrShape {
    /// One element per write along an unlimited axis, `chunk` deep.
    Series { chunk: usize },
    /// The extra-dimension grid `dims` (outermost first), unlimited on every
    /// axis, filled in the detector dataset's odometer order.
    Grid { dims: Vec<usize>, chunk: Vec<usize> },
    /// A Single file's rank-0 dataset: the frame has no frame axis.
    Scalar,
}

/// Where an attribute dataset's per-frame elements go.
enum AttrSink {
    /// Standard path: the little-endian elements at their row-major
    /// positions, `fill` between, written whole at close.
    Buffer(Vec<u8>),
    /// SWMR path: the writer's streaming dataset, created before `start_swmr`
    /// as C creates it (NDFileHDF5.cpp:297-326) and appended frame by frame.
    Swmr(usize),
    /// SWMR path of an [`AttrShape::Grid`]: the writer's grid dataset, the
    /// whole grid (fill where nothing landed), and the chunks changed since
    /// [`flush_swmr_writer`] last wrote them.
    SwmrGrid {
        index: usize,
        cells: Vec<u8>,
        dirty: std::collections::BTreeSet<Vec<usize>>,
    },
    /// SWMR path of an [`AttrShape::Scalar`]: C's contiguous rank-0 dataset,
    /// rewritten whole by each element.
    SwmrScalar(usize),
}

/// The positioned `cells` of a row-major grid of `dims`, `fill` to its end.
fn grid_cells(cells: &[u8], dims: &[usize], fill: &[u8]) -> Vec<u8> {
    let total: usize = dims.iter().product();
    let mut all = cells.to_vec();
    all.extend(fill.repeat(total.saturating_sub(cells.len() / fill.len())));
    all
}

/// The chunk at `coords` of the grid `cells`, `fill` past the grid's edge.
fn grid_tile(
    cells: &[u8],
    dims: &[usize],
    chunk: &[usize],
    coords: &[usize],
    fill: &[u8],
) -> Vec<u8> {
    let es = fill.len();
    let n: usize = chunk.iter().product();
    let mut tile = Vec::with_capacity(n * es);
    for i in 0..n {
        let at = Hdf5Writer::unravel(i, chunk)
            .iter()
            .zip(coords.iter().zip(chunk).zip(dims))
            .try_fold(0, |lin, (&p, ((&c, &ch), &d))| {
                let x = c * ch + p;
                (x < d).then_some(lin * d + x)
            });
        match at {
            Some(lin) => tile.extend_from_slice(&cells[lin * es..(lin + 1) * es]),
            None => tile.extend_from_slice(fill),
        }
    }
    tile
}

/// The one way the SWMR writer flushes: the grid chunks changed since the
/// last flush are written first, so a reader never sees a grid lag its data.
fn flush_swmr_writer(
    attr_datasets: &mut [AttributeDataset],
    writer: &mut SwmrFileWriter,
) -> ADResult<()> {
    let err = |e: rust_hdf5::Hdf5Error| {
        ADError::UnsupportedConversion(format!("SWMR attribute grid error: {e}"))
    };
    for ad in attr_datasets.iter_mut() {
        let fill = ad.fill();
        let (
            AttrShape::Grid { dims, chunk },
            AttrSink::SwmrGrid {
                index,
                cells,
                dirty,
            },
        ) = (&ad.shape, &mut ad.sink)
        else {
            continue;
        };
        for coords in std::mem::take(dirty) {
            let tile = grid_tile(cells, dims, chunk, &coords, &fill);
            let coords: Vec<u64> = coords.iter().map(|&c| c as u64).collect();
            writer.write_chunk_at(*index, &coords, &tile).map_err(err)?;
        }
        let extent: Vec<u64> = ad.extent.iter().map(|&d| d as u64).collect();
        writer.set_extent(*index, &extent).map_err(err)?;
    }
    writer
        .flush()
        .map_err(|e| ADError::UnsupportedConversion(format!("SWMR flush error: {e}")))
}

/// C `writeAttributeDataset(whenToSave, positionMode, offsets)`
/// (NDFileHDF5.cpp:2927-2995): every dataset saved at `when` takes one
/// element, read out of the sticky file list (`pFileAttributes->find`, :2957);
/// a miss is skipped as C's `continue`. With the frame's `offsets` the element
/// goes to its position, else to the next odometer slot. `swmr` is the writer
/// that owns every `AttrSink::Swmr` dataset.
fn write_attribute_datasets(
    attr_datasets: &mut [AttributeDataset],
    file_attributes: &FileAttributes,
    when: LayoutWhen,
    offsets: Option<&[usize]>,
    mut swmr: Option<&mut SwmrFileWriter>,
) -> ADResult<()> {
    for ad in attr_datasets.iter_mut().filter(|ad| ad.when == when) {
        let Some(attr) = file_attributes.get(&ad.name) else {
            continue;
        };
        let element = ad.element(&attr.value);
        let at = offsets.and_then(|o| ad.position(o));
        ad.put(&element, at, swmr.as_deref_mut())?;
    }
    Ok(())
}

/// C's attribute dataset starts at extent 1 and its first write fills that
/// slot (NDFileHDF5AttributeDataset.cpp:255), so one never written closes
/// holding a single fill element.
fn pad_unwritten_attribute_datasets(
    attr_datasets: &mut [AttributeDataset],
    mut swmr: Option<&mut SwmrFileWriter>,
) -> ADResult<()> {
    for ad in attr_datasets.iter_mut().filter(|ad| ad.count == 0) {
        let fill = ad.fill();
        ad.put(&fill, None, swmr.as_deref_mut())?;
    }
    Ok(())
}

/// C `NDAttribute` source-type string (NDAttribute.cpp:49-62), recorded as the
/// `NDAttrSourceType` HDF5 attribute on each NDAttribute dataset.
fn nd_attr_source_type_string(source: &NDAttrSource) -> &'static str {
    match source {
        NDAttrSource::Driver => "NDAttrSourceDriver",
        NDAttrSource::Param { .. } => "NDAttrSourceParam",
        NDAttrSource::EpicsPV(_) => "NDAttrSourceEPICSPV",
        NDAttrSource::Function(_) => "NDAttrSourceFunct",
        NDAttrSource::Constant(_) => "NDAttrSourceConst",
        NDAttrSource::Undefined => "Undefined",
    }
}

impl AttributeDataset {
    fn new(
        attr: &NDAttribute,
        group: String,
        dset_name: String,
        when: LayoutWhen,
        shape: AttrShape,
        offset_axes: Vec<usize>,
    ) -> Self {
        // C decides the HDF5 datatype and the fill value ONCE, from the
        // attribute's type at file-open: `createDataset` calls `typeAsHdf`
        // (NDFileHDF5AttributeDataset.cpp:73-97) and the type came from
        // `ndAttr->getDataType()` when the node was constructed
        // (NDFileHDF5.cpp:2797, :2830). Nothing re-runs it, so a concrete
        // value arriving in a later frame cannot change either. An attribute
        // with no value takes the `default:` arm — `H5T_NATIVE_FLOAT` with an
        // `epicsNAN` fill (`:366-370`) — not the type of whichever value
        // happens to arrive first.
        let undefined = matches!(attr.value, NDAttrValue::Undefined);
        Self {
            name: attr.name.clone(),
            description: attr.description.clone(),
            source: attr.source.source_string().to_string(),
            source_type: nd_attr_source_type_string(&attr.source).to_string(),
            data_type: if undefined {
                NDAttrDataType::Float32
            } else {
                attr.value.data_type()
            },
            undefined,
            group,
            dset_name,
            when,
            extent: match &shape {
                AttrShape::Series { .. } => vec![1],
                AttrShape::Grid { dims, .. } => vec![1; dims.len()],
                AttrShape::Scalar => Vec::new(),
            },
            shape,
            offset_axes,
            sink: AttrSink::Buffer(Vec::new()),
            count: 0,
        }
    }

    /// This dataset's element position for a frame at `offsets`, or `None`
    /// when the frame does not place it.
    fn position(&self, offsets: &[usize]) -> Option<Vec<usize>> {
        if self.offset_axes.is_empty() {
            return None;
        }
        self.offset_axes
            .iter()
            .map(|&i| offsets.get(i).copied())
            .collect()
    }

    /// Write `element` at `at`, or at the next odometer slot (C
    /// `extendDataSet()`, NDFileHDF5AttributeDataset.cpp:376-412).
    fn put(
        &mut self,
        element: &[u8],
        at: Option<Vec<usize>>,
        swmr: Option<&mut SwmrFileWriter>,
    ) -> ADResult<()> {
        let coords = at.unwrap_or_else(|| match &self.shape {
            AttrShape::Series { .. } => vec![self.count],
            AttrShape::Grid { dims, .. } => {
                Hdf5Writer::unravel(self.count % dims.iter().product::<usize>(), dims)
            }
            AttrShape::Scalar => Vec::new(),
        });
        let cell = match &self.shape {
            AttrShape::Series { .. } => coords[0],
            AttrShape::Grid { dims, .. } => {
                if coords.iter().zip(dims).any(|(c, d)| c >= d) {
                    return Err(ADError::UnsupportedConversion(format!(
                        "attribute {} position {coords:?} is past its grid {dims:?}",
                        self.name
                    )));
                }
                coords.iter().zip(dims).fold(0, |l, (c, d)| l * d + c)
            }
            AttrShape::Scalar => 0,
        };
        for (e, c) in self.extent.iter_mut().zip(&coords) {
            *e = (*e).max(c + 1);
        }
        let fill = self.fill();
        let es = element.len();
        match (&mut self.sink, swmr) {
            (AttrSink::Buffer(cells), _) => {
                while cells.len() <= cell * es {
                    cells.extend_from_slice(&fill);
                }
                cells[cell * es..(cell + 1) * es].copy_from_slice(element);
            }
            // A stream only appends; SWMR fixes PositionMode at open, so a
            // positioned series is a grid.
            (AttrSink::Swmr(index), Some(writer)) => {
                writer.append_frame(*index, element).map_err(|e| {
                    ADError::UnsupportedConversion(format!("SWMR attribute append error: {e}"))
                })?
            }
            // Its chunk is written by the next `flush_swmr_writer`.
            (AttrSink::SwmrGrid { cells, dirty, .. }, _) => {
                let AttrShape::Grid { chunk, .. } = &self.shape else {
                    unreachable!("a SwmrGrid sink holds a Grid shape");
                };
                cells[cell * es..(cell + 1) * es].copy_from_slice(element);
                dirty.insert(coords.iter().zip(chunk).map(|(p, c)| p / c).collect());
            }
            (AttrSink::SwmrScalar(index), Some(writer)) => {
                writer.write_dataset_raw(*index, element).map_err(|e| {
                    ADError::UnsupportedConversion(format!("SWMR attribute write error: {e}"))
                })?
            }
            (AttrSink::Swmr(_) | AttrSink::SwmrScalar(_), None) => {
                return Err(ADError::UnsupportedConversion(
                    "SWMR attribute dataset written without its writer".into(),
                ));
            }
        }
        self.count += 1;
        Ok(())
    }

    /// Dataset path, without the leading `/`.
    fn path(&self) -> String {
        if self.group.is_empty() {
            self.dset_name.clone()
        } else {
            format!("{}/{}", self.group, self.dset_name)
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

    /// The HDF5 datatype C's `typeAsHdf` picks (NDFileHDF5AttributeDataset.cpp:
    /// 313-372).
    fn datatype(&self) -> rust_hdf5::format::messages::datatype::DatatypeMessage {
        use rust_hdf5::types::H5Type;
        match self.data_type {
            NDAttrDataType::Int8 => i8::hdf5_type(),
            NDAttrDataType::UInt8 => u8::hdf5_type(),
            NDAttrDataType::Int16 => i16::hdf5_type(),
            NDAttrDataType::UInt16 => u16::hdf5_type(),
            NDAttrDataType::Int32 => i32::hdf5_type(),
            NDAttrDataType::UInt32 => u32::hdf5_type(),
            NDAttrDataType::Int64 => i64::hdf5_type(),
            NDAttrDataType::UInt64 => u64::hdf5_type(),
            NDAttrDataType::Float32 => f32::hdf5_type(),
            NDAttrDataType::Float64 => f64::hdf5_type(),
            NDAttrDataType::String => FixedStr256::hdf5_type(),
        }
    }

    /// The fill `typeAsHdf` installs: 0, or `epicsNAN` for an undefined
    /// attribute.
    fn fill(&self) -> Vec<u8> {
        if self.undefined {
            f32::NAN.to_le_bytes().to_vec()
        } else {
            vec![0u8; self.element_size()]
        }
    }

    /// One frame's value, encoded to the dataset's native type.
    fn element(&self, value: &NDAttrValue) -> Vec<u8> {
        if self.undefined {
            // C never writes an element into an undefined dataset, so the
            // `epicsNAN` fill installed by `H5Pset_fill_value`
            // (NDFileHDF5AttributeDataset.cpp:106) stands for every frame.
            // The port emits the fill explicitly; on disk and to a reader the
            // two are the same bytes.
            return self.fill();
        }
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
        bytes
    }
}

/// Fixed string field width for string-typed attribute datasets
/// (C++ `MAX_ATTRIBUTE_STRING_SIZE`).
const MAX_ATTRIBUTE_STRING_SIZE: usize = 256;

/// Element marker for a 256-byte fixed-length HDF5 string attribute dataset.
///
/// C `NDFileHDF5AttributeDataset.cpp:321-323` stores a string-valued
/// NDAttribute as a rank-1 dataset whose element datatype is `H5T_C_S1` sized
/// to `MAX_ATTRIBUTE_STRING_SIZE` bytes (null-terminated, ASCII). Implementing
/// `H5Type` so `hdf5_type()` returns that fixed-length string datatype lets the
/// high-level dataset builder emit `H5T_STR_NULLTERM` strings rather than a 2-D
/// `H5T_STD_U8LE` byte array. The element is byte-identical to the raw 256-byte
/// field already accumulated per frame, so the existing byte-oriented write
/// path is reused unchanged.
#[derive(Clone, Copy)]
#[repr(transparent)]
struct FixedStr256([u8; MAX_ATTRIBUTE_STRING_SIZE]);

impl rust_hdf5::types::H5Type for FixedStr256 {
    fn hdf5_type() -> rust_hdf5::format::messages::datatype::DatatypeMessage {
        rust_hdf5::format::messages::datatype::DatatypeMessage::fixed_string(
            MAX_ATTRIBUTE_STRING_SIZE as u32,
        )
    }

    fn element_size() -> usize {
        MAX_ATTRIBUTE_STRING_SIZE
    }
}

/// Write state for one detector-source dataset (C `NDFileHDF5Dataset` held in
/// `detDataMap`). Each `<dataset source="detector">` node gets its own live
/// dataset handle, leading-axis frame counter, and partial chunk band so
/// `detector_data_destination` routing can send frames to different datasets,
/// each extending independently. The on-disk datatype/dataspace/frame shape are
/// shared (set from the first frame) and live on `Hdf5Writer`.
struct DetectorDataset {
    /// Live dataset handle, retained across frames so the leading dimension can
    /// be extended (`H5File::dataset` cannot re-open a dataset in write mode).
    ds: rust_hdf5::H5Dataset,
    /// Frames routed to THIS dataset — its leading-axis index and final extent.
    frame_count: usize,
    /// LE bytes of frames buffered for THIS dataset until a `nFramesChunks`-deep
    /// chunk band fills. With one frame per chunk this holds at most one frame.
    frame_band: Vec<Vec<u8>>,
    /// The leading axes' extent, C `dims_`: 1 each until a frame lands
    /// further out.
    extent: Vec<usize>,
}

/// Internal handle: either a standard H5File or a SWMR streaming writer.
enum Hdf5Handle {
    Standard {
        file: H5File,
        /// Detector image datasets keyed by C full name (leading slash) — the
        /// C `detDataMap`. Created at open; one entry for the
        /// common single-`<dataset source="detector">` layout.
        detectors: std::collections::HashMap<String, DetectorDataset>,
    },
    Swmr {
        // Boxed: `SwmrFileWriter` is much larger than the `Standard` variant.
        writer: Box<SwmrFileWriter>,
        ds_index: usize,
        /// True only when a compression type was requested but no filter
        /// pipeline could be built for it; false when compression is applied.
        compression_dropped: bool,
        /// `Some` when the streaming dataset is a fixed multi-extra-dimension
        /// grid (`HDF5_nExtraDims >= 1`): frames are placed at
        /// odometer positions via `write_chunk_at`. `None` for the common
        /// single frame-axis case, which streams via `append_frame`.
        grid: Option<SwmrGridLayout>,
        /// The performance `timestamp` dataset, when StorePerform was set at
        /// open.
        perf: Option<usize>,
    },
}

/// Fixed multi-extra-dimension grid layout for the SWMR write path. The
/// streaming dataset is a `create_grid_dataset` of full shape
/// `[eds[N], …, eds[0], Y, X]`; each frame is written at its odometer chunk
/// position with `SwmrFileWriter::write_chunk_at`, mirroring the standard
/// path's [`Hdf5Writer::flush_band`].
struct SwmrGridLayout {
    /// Fixed leading axes, outermost-first (`extra_dim_axes`).
    leading: Vec<usize>,
    /// HDF5-order frame dims `[Y, X]` (fastest-varying last).
    frame_dims: Vec<usize>,
    /// Full per-chunk shape `[leading chunk…, rc, cc]` (same rank as the
    /// dataset).
    chunk: Vec<usize>,
    /// Element size in bytes.
    elem_size: usize,
    /// The leading axes' extent, C `dims_`: 1 each until a frame lands
    /// further out.
    extent: Vec<usize>,
}

/// The HDF5 settings as C keeps them in the param library: the port's write
/// stores here without waiting on a file write in progress, and the writer
/// reads them where C reads its params (open, each frame, close).
#[derive(Clone)]
pub struct Hdf5Settings {
    compression_type: i32,
    z_compress_level: i32,
    szip_num_pixels: u32,
    nbit_precision: u32,
    nbit_offset: u32,
    jpeg_quality: u32,
    blosc_shuffle_type: i32,
    blosc_compressor: i32,
    blosc_compress_level: i32,
    chunk: ChunkConfig,
    n_extra_dims: usize,
    extra_dims: [ExtraDim; MAX_EXTRA_DIMS],
    fill_value: f64,
    dim_att_datasets: bool,
    /// C `HDF5_posRunning`: each frame goes where its `HDF5_posNameDim*`
    /// NDAttributes say (NDFileHDF5.cpp:1480-1517).
    pos_running: bool,
    pos_names: [String; MAX_EXTRA_DIMS],
    pos_index_names: [String; MAX_EXTRA_DIMS],
    swmr_mode: bool,
    flush_nth_frame: usize,
    store_attributes: bool,
    store_performance: bool,
    /// XMLFileName as written: a path, inline `<?xml`, or empty.
    layout_spec: String,
}

impl Default for Hdf5Settings {
    fn default() -> Self {
        Self {
            compression_type: 0,
            z_compress_level: 6,
            szip_num_pixels: 16,
            // C default precision=8 bit, offset=0 (NDFileHDF5.cpp:2342-2343):
            // a default-config N-bit request packs to 8 significant bits.
            nbit_precision: 8,
            nbit_offset: 0,
            jpeg_quality: 90,
            // C default bloscShuffleType=1 (byte shuffle), NDFileHDF5.cpp:2346.
            blosc_shuffle_type: 1,
            blosc_compressor: 0,
            blosc_compress_level: 5,
            chunk: ChunkConfig::default(),
            n_extra_dims: 0,
            extra_dims: Default::default(),
            fill_value: 0.0,
            dim_att_datasets: false,
            pos_running: false,
            pos_names: Default::default(),
            pos_index_names: Default::default(),
            swmr_mode: false,
            flush_nth_frame: 0,
            store_attributes: true,
            store_performance: false,
            layout_spec: String::new(),
        }
    }
}

impl Hdf5Settings {
    /// Build a FilterPipeline from the current compression settings.
    fn build_pipeline(&self, element_size: usize, frame: &NDArray) -> Option<FilterPipeline> {
        match self.compression_type {
            COMPRESS_NONE => None,
            // H5Pset_deflate refuses a level past 9, -1 included as unsigned
            // (H5Pocpl.c:1229), and C sets no filter but keeps the zlib codec.
            COMPRESS_ZLIB => u32::try_from(self.z_compress_level)
                .ok()
                .filter(|&level| level <= 9)
                .map(FilterPipeline::deflate),
            COMPRESS_SZIP => Some(FilterPipeline {
                filters: vec![Filter {
                    id: FILTER_SZIP,
                    flags: 0,
                    // C calls H5Pset_szip(cparms, H5_SZIP_NN_OPTION_MASK, szipNumPixels)
                    // (NDFileHDF5.cpp:3372): nearest-neighbor coding (mask 32), not
                    // entropy coding (mask 4). cd_values[1] is pixels_per_block; the
                    // rust-hdf5 SZIP filter defaults bits_per_pixel/pixels_per_scanline.
                    cd_values: vec![SZIP_NN_OPTION_MASK, self.szip_num_pixels],
                }],
            }),
            // C sets FILTER_LZ4 for both (NDFileHDF5.cpp:3422-3438).
            COMPRESS_LZ4 | COMPRESS_LZ4HDF5 => Some(FilterPipeline::lz4()),
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
            // C passes level, shuffle and compressor through as unsigned
            // (NDFileHDF5.cpp:3395-3397), so a -1 is 0xFFFFFFFF.
            COMPRESS_BLOSC => Some(FilterPipeline {
                filters: vec![Filter {
                    id: FILTER_BLOSC,
                    flags: 0,
                    cd_values: vec![
                        2,
                        2,
                        element_size as u32,
                        0,
                        self.blosc_compress_level as u32,
                        self.blosc_shuffle_type as u32,
                        self.blosc_compressor as u32,
                    ],
                }],
            }),
            // The nbit filter travels with its narrowed datatype
            // (NDFileHDF5.cpp:3362-3364), so `nbit_packing` builds both.
            COMPRESS_NBIT => None,
            COMPRESS_JPEG => {
                let [ncols, nrows, rgb] = Self::jpeg_geometry(frame)?;
                Some(FilterPipeline {
                    filters: vec![Filter {
                        id: FILTER_JPEG,
                        flags: 0,
                        cd_values: vec![self.jpeg_quality, ncols, nrows, rgb],
                    }],
                })
            }
            _ => None,
        }
    }

    /// C's JPEG `cds[1..4]`, `[ncols, nrows, rgb]`, for the frame
    /// `configureCompression` is handed (NDFileHDF5.cpp:3439-3457): only a 2-D
    /// Mono or 3-D RGB1 frame has them, and C sets no filter and no codec for
    /// any other.
    fn jpeg_geometry(frame: &NDArray) -> Option<[u32; 3]> {
        // C `getValue(NDAttrInt32, &colorMode)`, Mono when absent or a string.
        let mode = frame
            .attributes
            .get("ColorMode")
            .and_then(|a| a.value.as_i64())
            .map_or(NDColorMode::Mono as i32, |v| v as i32);
        let size = |i: usize| frame.dims[i].size as u32;
        match frame.dims.len() {
            2 if mode == NDColorMode::Mono as i32 => Some([size(0), size(1), 0]),
            3 if mode == NDColorMode::RGB1 as i32 => Some([size(1), size(2), 1]),
            _ => None,
        }
    }

    /// N-bit packing for the standard write path: the on-disk datatype
    /// override and the matching nbit filter, derived together.
    ///
    /// C narrows `this->datatype` via `H5Tset_precision`/`H5Tset_offset` and
    /// then registers a parameterless `H5Pset_nbit` (NDFileHDF5.cpp:3362-3364)
    /// whatever the setters refused; libhdf5's `set_local` callback reads the
    /// datatype to build the filter `cd_values`. Here `DatasetBuilder::datatype`
    /// supplies [`nbit_datatype`](Self::nbit_datatype) and `FilterPipeline::nbit`
    /// builds the matching `cd_values` tree `[nparms, need_not_compress,
    /// d_nelmts, NBIT_ATOMIC, size, order, precision, offset]`, so the dataset
    /// is byte-readable by h5py / libhdf5.
    ///
    /// Returns `None` unless the scheme is `COMPRESS_NBIT`.
    ///
    /// `chunk_nelmts` is the element count of one on-disk chunk (= `d_nelmts`);
    /// `flush_band` always writes a full, zero-padded chunk, so every filtered
    /// write supplies exactly `chunk_nelmts * element_size` bytes — the size
    /// `apply_nbit` requires.
    fn nbit_packing(
        &self,
        data_type: NDDataType,
        chunk_nelmts: usize,
    ) -> Option<(DatatypeMessage, FilterPipeline)> {
        let dt = self.nbit_datatype(data_type)?;
        let pipeline = FilterPipeline::nbit(&dt, chunk_nelmts);
        Some((dt, pipeline))
    }

    /// The datatype C's `H5Tset_precision` then `H5Tset_offset` leave on a
    /// copy of the native one (NDFileHDF5.cpp:3362-3363, :3887). HDF5 refuses
    /// precision 0 and a float precision that cuts its sign bit, leaving the
    /// native precision; a precision past the element drops the offset and
    /// grows the size (H5Tprecis.c `H5T_set_precision`). The offset always
    /// holds, growing the size if it must (H5Toffset.c `H5T_set_offset`).
    fn nbit_datatype(&self, data_type: NDDataType) -> Option<DatatypeMessage> {
        if self.compression_type != COMPRESS_NBIT {
            return None;
        }
        let mut dt = native_datatype(data_type);
        let float = matches!(data_type, NDDataType::Float32 | NDDataType::Float64);
        let (DatatypeMessage::FixedPoint {
            size,
            bit_offset,
            bit_precision,
            ..
        }
        | DatatypeMessage::FloatingPoint {
            size,
            bit_offset,
            bit_precision,
            ..
        }) = &mut dt
        else {
            unreachable!("native numeric types are atomic")
        };
        let p = self.nbit_precision;
        if p != 0 && !(float && p < *size * 8) {
            if p > *size * 8 {
                *size = p.div_ceil(8);
            }
            *bit_precision = p as u16;
        }
        let offset = self.nbit_offset;
        if offset + u32::from(*bit_precision) > *size * 8 {
            *size = (offset + u32::from(*bit_precision)).div_ceil(8);
        }
        *bit_offset = offset as u16;
        Some(dt)
    }

    /// The params C `configureCompression` sets from a pre-compressed frame
    /// whose codec it names (NDFileHDF5.cpp:3316-3337), before it reads them
    /// back to build the dataset.
    fn codec_override(codec: Option<&Codec>) -> Vec<(CompressionParam, i32)> {
        use CompressionParam::*;
        let Some(c) = codec else {
            return Vec::new();
        };
        match c.name {
            CodecName::Blosc => vec![
                (Type, COMPRESS_BLOSC),
                (BloscLevel, c.level),
                (BloscShuffle, c.shuffle),
                (BloscCompressor, c.compressor),
            ],
            CodecName::Zlib => vec![(Type, COMPRESS_ZLIB), (ZLevel, c.level)],
            CodecName::BSLZ4 => vec![(Type, COMPRESS_BSHUF)],
            CodecName::LZ4 => vec![(Type, COMPRESS_LZ4)],
            CodecName::LZ4HDF5 => vec![(Type, COMPRESS_LZ4HDF5)],
            CodecName::JPEG => vec![(Type, COMPRESS_JPEG)],
            CodecName::None | CodecName::Other(_) => Vec::new(),
        }
    }

    fn overridden(&self, params: &[(CompressionParam, i32)]) -> Self {
        let mut s = self.clone();
        s.apply(params);
        s
    }

    /// The nbit offset and precision C reads back after setting them
    /// (NDFileHDF5.cpp:3366-3371).
    fn nbit_readback(&self, data_type: NDDataType) -> Vec<(CompressionParam, i32)> {
        let Some(
            DatatypeMessage::FixedPoint {
                bit_offset,
                bit_precision,
                ..
            }
            | DatatypeMessage::FloatingPoint {
                bit_offset,
                bit_precision,
                ..
            },
        ) = self.nbit_datatype(data_type)
        else {
            return Vec::new();
        };
        vec![
            (CompressionParam::NbitOffset, i32::from(bit_offset)),
            (CompressionParam::NbitPrecision, i32::from(bit_precision)),
        ]
    }

    fn apply(&mut self, params: &[(CompressionParam, i32)]) {
        for &(param, v) in params {
            match param {
                CompressionParam::Type => self.compression_type = v,
                CompressionParam::ZLevel => self.z_compress_level = v,
                CompressionParam::BloscLevel => self.blosc_compress_level = v,
                CompressionParam::BloscShuffle => self.blosc_shuffle_type = v,
                CompressionParam::BloscCompressor => self.blosc_compressor = v,
                CompressionParam::NbitOffset => self.nbit_offset = v as u32,
                CompressionParam::NbitPrecision => self.nbit_precision = v as u32,
            }
        }
    }

    /// C `this->codec` for this scheme (NDFileHDF5.cpp:3352-3469): what
    /// `verifyChunking` holds every frame to.
    fn dataset_codec(&self, data_type: NDDataType, frame: &NDArray) -> Option<Codec> {
        let unset = Codec::UNSET;
        let (name, level, shuffle, compressor) = match self.compression_type {
            COMPRESS_NBIT => (CodecName::from_name("nbit"), unset, unset, unset),
            COMPRESS_SZIP => (CodecName::from_name("szip"), unset, unset, unset),
            COMPRESS_ZLIB => (CodecName::Zlib, self.z_compress_level, unset, unset),
            COMPRESS_BLOSC => (
                CodecName::Blosc,
                self.blosc_compress_level,
                self.blosc_shuffle_type,
                self.blosc_compressor,
            ),
            COMPRESS_BSHUF => (CodecName::BSLZ4, unset, unset, unset),
            COMPRESS_LZ4 => (CodecName::LZ4, unset, unset, unset),
            COMPRESS_LZ4HDF5 => (CodecName::LZ4HDF5, unset, unset, unset),
            COMPRESS_JPEG => {
                Self::jpeg_geometry(frame)?;
                (CodecName::JPEG, unset, unset, unset)
            }
            _ => return None,
        };
        Some(Codec {
            name,
            compressed_size: 0,
            level,
            shuffle,
            compressor,
            original_data_type: data_type,
        })
    }

    pub fn set_compression_type(&mut self, v: i32) {
        self.compression_type = v;
    }

    pub fn set_z_compress_level(&mut self, v: i32) {
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

    pub fn set_blosc_compress_level(&mut self, v: i32) {
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
        // `0` is the auto sentinel (C `HDF5_NDAttributeChunk` default); preserve
        // it rather than clamping to 1, so `attribute_chunking` can resolve it.
        self.chunk.ndattr_chunk = v;
    }

    pub fn set_n_extra_dims(&mut self, v: usize) {
        // C `NDFileHDF5::writeInt32` rejects `HDF5_nExtraDims > MAXEXTRADIMS-1`
        // (NDFileHDF5.cpp:1789). The dataset gains `nExtraDims+1` leading axes
        // (the virtual scan dims plus the innermost frames-per-point axis), so
        // axis i reads `extra_dims[nExtraDims - i]`; the highest index used is
        // `nExtraDims`, which must stay within `extra_dims[0..MAX_EXTRA_DIMS]`.
        self.n_extra_dims = v.min(MAX_EXTRA_DIMS - 1);
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

    pub fn set_extra_dim_chunk(&mut self, idx: usize, chunk: i32) {
        if idx < MAX_EXTRA_DIMS {
            self.extra_dims[idx].chunk = chunk;
        }
    }

    pub fn set_fill_value(&mut self, v: f64) {
        self.fill_value = v;
    }

    pub fn set_dim_att_datasets(&mut self, v: bool) {
        self.dim_att_datasets = v;
    }

    pub fn set_pos_running(&mut self, v: bool) {
        self.pos_running = v;
    }

    pub fn set_pos_name(&mut self, idx: usize, name: &str) {
        if idx < MAX_EXTRA_DIMS {
            self.pos_names[idx] = name.to_string();
        }
    }

    pub fn set_pos_index_name(&mut self, idx: usize, name: &str) {
        if idx < MAX_EXTRA_DIMS {
            self.pos_index_names[idx] = name.to_string();
        }
    }

    /// Store XMLFileName; `load_layout` reads it at the next open, as C
    /// re-reads the param there.
    pub fn set_layout_filename(&mut self, spec: &str) {
        self.layout_spec = spec.to_owned();
    }
}

/// HDF5 file writer using the rust-hdf5 crate.
pub struct Hdf5Writer {
    current_path: Option<PathBuf>,
    handle: Option<Hdf5Handle>,
    /// Total frames written to the file this open (C `nextRecord` across all
    /// detector datasets): drives the create-on-first-frame gate, the
    /// `frame_count()` accessor and the attribute-dataset row count. Per-dataset
    /// leading-axis placement uses each [`DetectorDataset`]'s own `frame_count`.
    frame_count: usize,
    dataset_name: String,
    /// Cached data type of the open primary dataset.
    open_data_type: Option<NDDataType>,
    /// Bytes per element on disk: an nbit datatype HDF5 widened is larger
    /// than `open_data_type` (see [`widen_le`]).
    stored_elem_size: usize,
    /// Cached spatial (per-frame) dimensions, fastest-varying last.
    open_frame_dims: Option<Vec<usize>>,
    /// C `NDFileHDF5Dataset::codec`: the codec of the open file's scheme,
    /// which `verifyChunking` requires of a frame to direct-chunk-write it.
    dataset_codec: Option<Codec>,
    /// Chunk geometry of the open detector dataset(s), leading axes first.
    open_chunk: Vec<usize>,
    /// What `configure_compression` left for the port to post and print;
    /// shared so the port drains it without the writer's lock.
    open_posts: Arc<Mutex<OpenPosts>>,
    /// The settings store the port writes into, and the copy of it this
    /// writer last read (C reads its params at open, per frame and at close).
    cfg: WriterSettings<Hdf5Settings>,
    /// C `multiFrameFile`, the open's Multiple bit (NDFileHDF5.cpp:224-231),
    /// and the configured capture target (C `NDFileNumCapture`, pushed by the
    /// controller before `open_file`). Both feed `attribute_chunking`: C reads
    /// NDFileWriteMode there, which is Single exactly when this writer, one
    /// that supports multiple arrays, is opened without Multiple.
    multi_frame: bool,
    num_capture: i32,
    pub swmr_cb_counter: u32,
    /// Whether to fsync on file close (durable). Default `true`; set
    /// `AD_HDF5_FSYNC_ON_CLOSE=0` (or `false`/`no`/`off`) to skip the
    /// close-time fsync on the standard (non-SWMR) write path, trading
    /// durability for a faster close (`H5File::close_no_sync`, rust-hdf5
    /// 0.3.2). SWMR is unaffected — its high-level writer exposes no
    /// no-fsync close, and it streams/flushes incrementally regardless.
    fsync_on_close: bool,
    pub total_runtime: f64,
    pub total_bytes: u64,
    /// Per-frame I/O timing rows for the `timestamp` performance dataset.
    /// Each row is the 5 doubles C++ `writePerformanceDataset` records.
    perf_rows: Vec<[f64; 5]>,
    perf_prev: Option<std::time::Instant>,
    perf_first: Option<std::time::Instant>,
    /// Open NDAttribute time-series datasets, keyed by attribute name.
    attr_datasets: Vec<AttributeDataset>,
    /// C `pFileAttributes` (NDFileHDF5.cpp:243, :1437): the writer's own
    /// attribute list, sticky for the life of the file.
    file_attributes: FileAttributes,
    /// The plugin's own set, C `getAttributes` in `openFile`/`writeFile`.
    plugin_attributes: NDAttributeList,
    /// The layout `layout_spec` gave at the last open.
    layout: Option<Hdf5Layout>,
    /// Full path of the primary image dataset for the currently-open file.
    /// `"data"` (flat root) when no valid layout is loaded; the layout's
    /// `det_default` dataset path (e.g. `entry/instrument/detector/data`)
    /// otherwise. Leading slash stripped — keyed as `rust-hdf5` keys datasets.
    resolved_dataset_path: String,
    /// Group (no leading/trailing slash; empty is the file root) of the
    /// NDAttribute datasets the layout does not place, `Hdf5Layout::ndattr_home`;
    /// `None` stores none.
    resolved_ndattr_group: Option<String>,
    /// Group of the performance dataset, `Hdf5Layout::performance_group`;
    /// `None` stores none.
    resolved_perf_group: Option<String>,
    /// C's `gethostname` at construction (NDFileHDF5.cpp:2413-2414), empty
    /// when it failed.
    hostname: String,
    /// Attribute names created in the open file.
    created_attrs: CreatedAttrs,
    /// Live NDAttribute values for the names referenced by layout
    /// `<attribute source="ndattribute">` element-attrs (ADP-79). `first` is
    /// the open-time frame (C `storeOnOpenAttributes` → OnFileOpen/OnFrame),
    /// `last` the most recent frame (C `storeOnCloseAttributes` → OnFileClose).
    /// Empty for the default layout, which declares no such element-attrs.
    ndattr_first_values: std::collections::HashMap<String, NDAttrValue>,
    ndattr_last_values: std::collections::HashMap<String, NDAttrValue>,
    /// Distinct NDAttribute names referenced by element-attrs, captured at
    /// open so per-frame updates need not re-walk the layout.
    ndattr_element_names: Vec<String>,
}

impl Hdf5Writer {
    pub fn new() -> Self {
        Self {
            current_path: None,
            handle: None,
            frame_count: 0,
            dataset_name: "data".to_string(),
            open_data_type: None,
            stored_elem_size: 0,
            open_frame_dims: None,
            dataset_codec: None,
            open_chunk: Vec::new(),
            open_posts: Arc::default(),
            cfg: WriterSettings::new(Hdf5Settings::default()),
            multi_frame: false,
            num_capture: 1,
            swmr_cb_counter: 0,
            fsync_on_close: Self::env_fsync_on_close(),
            total_runtime: 0.0,
            total_bytes: 0,
            perf_rows: Vec::new(),
            perf_prev: None,
            perf_first: None,
            attr_datasets: Vec::new(),
            file_attributes: FileAttributes::default(),
            plugin_attributes: NDAttributeList::new(),
            // No user layout XML by default → C's built-in NeXus DEFAULT_LAYOUT.
            layout: Some(Self::default_layout()),
            resolved_dataset_path: "data".to_string(),
            resolved_ndattr_group: None,
            resolved_perf_group: None,
            hostname: epics_base_rs::runtime::env::hostname()
                .map(|h| String::from_utf8_lossy(&h).into_owned())
                .unwrap_or_default(),
            created_attrs: CreatedAttrs::default(),
            ndattr_first_values: std::collections::HashMap::new(),
            ndattr_last_values: std::collections::HashMap::new(),
            ndattr_element_names: Vec::new(),
        }
    }

    /// Read `AD_HDF5_FSYNC_ON_CLOSE` to decide the close-time fsync policy.
    fn env_fsync_on_close() -> bool {
        Self::parse_fsync_on_close_env(std::env::var("AD_HDF5_FSYNC_ON_CLOSE").ok().as_deref())
    }

    /// Pure parse of the `AD_HDF5_FSYNC_ON_CLOSE` value. `None` (unset) or any
    /// value outside the falsey set keeps the durable default (`true`);
    /// `0` / `false` / `no` / `off` (trimmed, case-insensitive) opts into the
    /// no-fsync fast close on the standard write path.
    fn parse_fsync_on_close_env(v: Option<&str>) -> bool {
        match v {
            Some(s) => !matches!(
                s.trim().to_ascii_lowercase().as_str(),
                "0" | "false" | "no" | "off"
            ),
            None => true,
        }
    }

    /// The store this writer reads; the port writes settings through it.
    pub fn settings(&self) -> SettingsStore<Hdf5Settings> {
        self.cfg.store()
    }

    /// The params C `configureCompression` sets, which `openFile` runs for
    /// every file, SWMR included (NDFileHDF5.cpp:279, :3890): a frame's codec
    /// overrides the scheme, the settings read after it are the result, and
    /// an nbit scheme posts the datatype HDF5 reads back.
    fn configure_compression(&mut self, array: &NDArray) {
        let mut posts = Hdf5Settings::codec_override(array.codec.as_ref());
        let data_type = array
            .codec
            .as_ref()
            .map_or(array.data().data_type(), |c| c.original_data_type);
        posts.extend(self.cfg.overridden(&posts).nbit_readback(data_type));
        if !posts.is_empty() {
            self.cfg.store().update(|s| s.apply(&posts));
            self.read_settings();
        }
        let mut configured = self.open_posts.lock();
        configured
            .posts
            .extend(posts.into_iter().map(|(p, v)| OpenPost::Compression(p, v)));
        if self.cfg.compression_type == COMPRESS_JPEG
            && Hdf5Settings::jpeg_geometry(array).is_none()
        {
            configured
                .lines
                .push("JPEG compression only supports 2-D mono and 3-D RGB1 modes\n".to_owned());
        }
    }

    fn open_posts(&self) -> Arc<Mutex<OpenPosts>> {
        Arc::clone(&self.open_posts)
    }

    /// Read the settings where C reads its params: open, each frame, close.
    fn read_settings(&mut self) {
        self.cfg.refresh();
    }

    pub fn set_dataset_name(&mut self, name: &str) {
        self.dataset_name = name.to_string();
    }

    pub fn set_compression_type(&mut self, v: i32) {
        self.cfg.store().update(|s| s.set_compression_type(v));
    }

    pub fn set_z_compress_level(&mut self, v: i32) {
        self.cfg.store().update(|s| s.set_z_compress_level(v));
    }

    pub fn set_szip_num_pixels(&mut self, v: u32) {
        self.cfg.store().update(|s| s.set_szip_num_pixels(v));
    }

    pub fn set_blosc_shuffle_type(&mut self, v: i32) {
        self.cfg.store().update(|s| s.set_blosc_shuffle_type(v));
    }

    pub fn set_blosc_compressor(&mut self, v: i32) {
        self.cfg.store().update(|s| s.set_blosc_compressor(v));
    }

    pub fn set_blosc_compress_level(&mut self, v: i32) {
        self.cfg.store().update(|s| s.set_blosc_compress_level(v));
    }

    pub fn set_nbit_precision(&mut self, v: u32) {
        self.cfg.store().update(|s| s.set_nbit_precision(v));
    }

    pub fn set_nbit_offset(&mut self, v: u32) {
        self.cfg.store().update(|s| s.set_nbit_offset(v));
    }

    pub fn set_jpeg_quality(&mut self, v: u32) {
        self.cfg.store().update(|s| s.set_jpeg_quality(v));
    }

    pub fn set_store_attributes(&mut self, v: bool) {
        self.cfg.store().update(|s| s.set_store_attributes(v));
    }

    pub fn set_store_performance(&mut self, v: bool) {
        self.cfg.store().update(|s| s.set_store_performance(v));
    }

    pub fn set_swmr_mode(&mut self, v: bool) {
        self.cfg.store().update(|s| s.set_swmr_mode(v));
    }

    pub fn set_flush_nth_frame(&mut self, v: usize) {
        self.cfg.store().update(|s| s.set_flush_nth_frame(v));
    }

    pub fn set_chunk_size_auto(&mut self, v: bool) {
        self.cfg.store().update(|s| s.set_chunk_size_auto(v));
    }

    pub fn set_n_row_chunks(&mut self, v: usize) {
        self.cfg.store().update(|s| s.set_n_row_chunks(v));
    }

    pub fn set_n_col_chunks(&mut self, v: usize) {
        self.cfg.store().update(|s| s.set_n_col_chunks(v));
    }

    pub fn set_n_frames_chunks(&mut self, v: usize) {
        self.cfg.store().update(|s| s.set_n_frames_chunks(v));
    }

    pub fn set_ndattr_chunk(&mut self, v: usize) {
        self.cfg.store().update(|s| s.set_ndattr_chunk(v));
    }

    pub fn set_n_extra_dims(&mut self, v: usize) {
        self.cfg.store().update(|s| s.set_n_extra_dims(v));
    }

    pub fn set_extra_dim_size(&mut self, idx: usize, size: usize) {
        self.cfg.store().update(|s| s.set_extra_dim_size(idx, size));
    }

    pub fn set_extra_dim_name(&mut self, idx: usize, name: &str) {
        self.cfg.store().update(|s| s.set_extra_dim_name(idx, name));
    }

    pub fn set_extra_dim_chunk(&mut self, idx: usize, chunk: i32) {
        self.cfg
            .store()
            .update(|s| s.set_extra_dim_chunk(idx, chunk));
    }

    pub fn set_fill_value(&mut self, v: f64) {
        self.cfg.store().update(|s| s.set_fill_value(v));
    }

    pub fn set_dim_att_datasets(&mut self, v: bool) {
        self.cfg.store().update(|s| s.set_dim_att_datasets(v));
    }

    pub fn set_pos_running(&mut self, v: bool) {
        self.cfg.store().update(|s| s.set_pos_running(v));
    }

    pub fn set_pos_name(&mut self, idx: usize, name: &str) {
        self.cfg.store().update(|s| s.set_pos_name(idx, name));
    }

    pub fn set_pos_index_name(&mut self, idx: usize, name: &str) {
        self.cfg.store().update(|s| s.set_pos_index_name(idx, name));
    }

    /// Parse the built-in [`DEFAULT_LAYOUT_XML`] into an `Hdf5Layout`. The
    /// constant is known-good (covered by a unit test), so a parse failure is a
    /// build bug, not a runtime condition.
    fn default_layout() -> Hdf5Layout {
        Hdf5Layout::parse(DEFAULT_LAYOUT_XML).expect("built-in default layout must parse")
    }

    pub fn set_layout_filename(&mut self, spec: &str) {
        self.cfg.store().update(|s| s.set_layout_filename(spec));
    }

    /// C `createFileLayout` (NDFileHDF5.cpp:3898-3939): the default layout
    /// for an empty XMLFileName, inline XML or an existing file parsed, and
    /// the open fails on a parse error or a missing file.
    fn load_layout(&mut self) -> ADResult<()> {
        self.read_settings();
        let spec = self.cfg.layout_spec.as_str();
        // A parse error prints only under HDF5_LOGGING (NDFileHDF5LayoutXML.h:34-41).
        let mut lines = Vec::new();
        let loaded = if spec.is_empty() {
            Ok(Self::default_layout())
        } else if Hdf5Layout::is_inline(spec) || Path::new(spec).exists() {
            Hdf5Layout::load(spec).map_err(|e| e.0)
        } else {
            lines.push(
                "NDFileHDF5createFileLayout Warning: specified XML file does not exist\n"
                    .to_owned(),
            );
            Err("specified XML file does not exist".to_owned())
        };
        match loaded {
            Ok(layout) => {
                self.layout = Some(layout);
                Ok(())
            }
            Err(e) => {
                self.layout = None;
                Err(layout_failed(lines, &e))
            }
        }
    }

    /// The default NDAttribute group C `createAttributeDataset` names the
    /// host on, ignoring a failure (NDFileHDF5.cpp:2768-2769): after the
    /// layout is accepted, so a refused file has none.
    fn hostname_group(&self) -> Option<&str> {
        self.resolved_ndattr_group
            .as_deref()
            .filter(|p| !p.is_empty() && self.cfg.store_attributes && !self.hostname.is_empty())
    }

    /// The layout's detector datasets (C `detDataMap`), or C
    /// `createXMLFileLayout`'s refusal of a layout without one
    /// (NDFileHDF5.cpp:471-491), made after `createTree` built the rest (:463).
    /// A root detector dataset counts for none: C takes it as the default but
    /// never creates it, and its first `writeFile` dereferences the missing
    /// `detDataMap` entry (:1501, a C defect not copied).
    fn layout_detector_paths(layout: &Hdf5Layout) -> ADResult<Vec<String>> {
        let paths = layout.detector_dataset_paths();
        if paths.is_empty() {
            return Err(layout_failed(
                vec![
                    "NDFileHDF5::createXMLFileLayout No detector datasets found, cannot continue\n"
                        .to_owned(),
                ],
                "No detector datasets found",
            ));
        }
        Ok(paths)
    }

    /// C `createNewFile`'s failure (NDFileHDF5.cpp:3846-3851) as `openFile`
    /// reports it (:271-275).
    fn create_failed(path: &Path, e: impl std::fmt::Display) -> ADError {
        ADError::Reported {
            lines: vec![
                format!(
                    "NDFileHDF5::createNewFile Unable to create HDF5 file: {}\n",
                    path.display()
                ),
                "NDFileHDF5::openFile ERROR Failed to create a new output file\n".to_owned(),
            ],
            message: format!("HDF5 create error: {e}"),
        }
    }

    pub fn frame_count(&self) -> usize {
        self.frame_count
    }

    /// Trigger a SWMR flush. No-op if not in SWMR mode.
    pub fn flush_swmr(&mut self) {
        if let Some(Hdf5Handle::Swmr { ref mut writer, .. }) = self.handle {
            if flush_swmr_writer(&mut self.attr_datasets, writer).is_ok() {
                self.swmr_cb_counter += 1;
            }
        }
    }

    /// Returns true if SWMR is currently active.
    pub fn is_swmr_active(&self) -> bool {
        matches!(self.handle, Some(Hdf5Handle::Swmr { .. }))
    }

    /// Whether a requested SWMR compression type had no buildable pipeline.
    pub fn swmr_compression_dropped(&self) -> bool {
        matches!(
            self.handle,
            Some(Hdf5Handle::Swmr {
                compression_dropped: true,
                ..
            })
        )
    }

    /// Chunk dims for the frame (image) axes only — no leading dims.
    ///
    /// For a 2-D frame these are the row/column chunk sizes
    /// (`HDF5_nRowChunks` / `HDF5_nColChunks`; 0, auto, or out-of-range → the
    /// full dimension, matching C++ `NDFileHDF5` chunk-size selection). Other
    /// ranks get one full per-frame tile (no sub-tiling).
    fn frame_chunk_dims(&self, frame_dims: &[usize]) -> Vec<usize> {
        if frame_dims.len() == 2 {
            let y = frame_dims[0].max(1);
            let x = frame_dims[1].max(1);
            vec![
                Self::clamp_chunk(self.cfg.chunk.n_row_chunks, y, self.cfg.chunk.auto),
                Self::clamp_chunk(self.cfg.chunk.n_col_chunks, x, self.cfg.chunk.auto),
            ]
        } else {
            frame_dims.iter().map(|&d| d.max(1)).collect()
        }
    }

    /// The fixed leading extra-dimension axes, outermost-first, for the
    /// standard (non-SWMR) write path; `None` when `HDF5_nExtraDims == 0`.
    ///
    /// `HDF5_nExtraDims = N` produces `N+1` leading axes: the `N` virtual scan
    /// dimensions plus the innermost frames-per-point axis. C builds them in
    /// reverse param order (NDFileHDF5.cpp:3182-3215, doc order
    /// `{Nth virtual, …, Y, X, frames-per-point, frame Y, frame X}`): axis `i`
    /// uses `extraDimSize[N - i]`, so outermost is `extraDimSize[N]` and the
    /// innermost leading axis is `extraDimSize[0]` ("N", frames per point).
    fn extra_dim_axes(&self) -> Option<Vec<usize>> {
        (self.cfg.n_extra_dims > 0).then(|| self.virtual_dims())
    }

    /// C `virtualdims` (NDFileHDF5Dataset.cpp:85-92): every extra dimension's
    /// size, outermost first, the frames-per-point axis included.
    fn virtual_dims(&self) -> Vec<usize> {
        (0..=self.cfg.n_extra_dims)
            .rev()
            .map(|i| self.cfg.extra_dims[i].size.max(1))
            .collect()
    }

    /// C's chunk on each extra axis, outermost first: `extraDimChunk`
    /// clamped to `1..=size` (NDFileHDF5.cpp:3184-3200), the innermost then
    /// replaced by nFramesChunks (:3266-3274).
    fn extra_dim_chunks(&self) -> Vec<usize> {
        (0..=self.cfg.n_extra_dims)
            .rev()
            .map(|i| match &self.cfg.extra_dims[i] {
                _ if i == 0 => self.cfg.chunk.n_frames_chunks.max(1),
                d => (d.chunk.max(1) as usize).min(d.size.max(1)),
            })
            .collect()
    }

    /// C `writeFile`'s positional placement (NDFileHDF5.cpp:1480-1494,
    /// `findPositionIndex` :3974-3987): the frame's offset on each extra
    /// dimension, outermost first, read as Int32 from the NDAttribute its
    /// `HDF5_posNameDim*` names. A missing attribute or a negative value fails
    /// the frame.
    fn frame_positions(&self, array: &NDArray) -> ADResult<Option<Vec<usize>>> {
        if !self.cfg.pos_running {
            return Ok(None);
        }
        let n = self.cfg.n_extra_dims;
        let mut offsets = vec![0; n + 1];
        for index in 0..=n {
            let name = &self.cfg.pos_names[index];
            let Some(attr) = array.attributes.get(name) else {
                return Err(ADError::Reported {
                    lines: vec![format!(
                        "NDFileHDF5::findPositionIndex ERROR: could not find position \
                         attribute {name} in list. Aborting\n"
                    )],
                    message: format!("could not find position attribute {name}"),
                });
            };
            // C `getValue(NDAttrInt32)` leaves the -1 for a string.
            let pos = attr.value.as_i64().map_or(-1, |v| v as i32);
            offsets[n - index] = usize::try_from(pos).map_err(|_| ADError::Reported {
                lines: Vec::new(),
                message: format!("position attribute {name} is {pos}"),
            })?;
        }
        Ok(Some(offsets))
    }

    /// C `NDFileHDF5Dataset::extendDataSet(extradims, offsets)`
    /// (NDFileHDF5Dataset.cpp:161-185): an offset past its extra dimension
    /// fails the frame, with a line for each such axis.
    fn check_positions(&self, offsets: &[usize], dataset: &str) -> ADResult<()> {
        let lines: Vec<String> = offsets
            .iter()
            .zip(self.virtual_dims())
            .filter(|&(&o, v)| o >= v)
            .map(|_| {
                format!("NDFileHDF5Dataset::extendDataSet ERROR extending the dataset [{dataset}] failed\n")
            })
            .collect();
        if lines.is_empty() {
            return Ok(());
        }
        Err(ADError::Reported {
            lines,
            message: format!("extending the dataset [{dataset}] failed"),
        })
    }

    /// Standard (non-SWMR) dataset shape and chunk geometry for the primary
    /// image dataset, faithful to C `NDFileHDF5::configureDims`.
    ///
    /// Layout, fastest-varying last. A Single file is the frame alone, `[Y, X]`
    /// (C `multiFrameFile` is false, so `extradims = 0`). With no extra dims it
    /// is `[frame, Y, X]` and the single leading axis is extensible. With
    /// `HDF5_nExtraDims = N`
    /// it is `[eds[N], …, eds[1], eds[0], Y, X]` — `N+1` fixed leading axes
    /// created at full configured size, chunked as `extra_dim_chunks` (the
    /// frame data is row-major identical to the collapsed form, only the
    /// dataspace rank differs). `close_file` calls `set_extent` to the extent C's
    /// `extendDataSet` reaches for the frames written.
    ///
    /// Returns `(shape, chunk, leading)` where `leading` is `Some([eds…])`
    /// (the fixed leading axes, outermost-first; none in a Single file) when
    /// they are fixed up front, or `None` when the single leading frame axis
    /// is extended per write.
    fn standard_layout(
        &self,
        frame_dims: &[usize],
    ) -> (Vec<usize>, Vec<usize>, Option<Vec<usize>>) {
        let frame_chunk = self.frame_chunk_dims(frame_dims);
        if !self.multi_frame {
            return (frame_dims.to_vec(), frame_chunk, Some(vec![]));
        }
        match self.extra_dim_axes() {
            Some(leading) => {
                let mut shape = leading.clone();
                shape.extend_from_slice(frame_dims);
                let mut chunk = self.extra_dim_chunks();
                chunk.extend_from_slice(&frame_chunk);
                (shape, chunk, Some(leading))
            }
            None => {
                let mut shape = vec![1usize];
                shape.extend_from_slice(frame_dims);
                let mut chunk = vec![self.cfg.chunk.n_frames_chunks.max(1)];
                chunk.extend_from_slice(&frame_chunk);
                (shape, chunk, None)
            }
        }
    }

    /// C maxdims (NDFileHDF5.cpp:3196-3218) of the `standard_layout` shape:
    /// a single frame axis is NumCapture, unlimited when that is 0; every
    /// other axis is its size.
    fn detector_max_shape(&self, shape: &[usize], frame_axis: bool) -> Vec<Option<usize>> {
        shape
            .iter()
            .enumerate()
            .map(|(i, &s)| {
                if frame_axis && i == 0 {
                    (self.num_capture > 0).then_some(self.num_capture as usize)
                } else {
                    Some(s)
                }
            })
            .collect()
    }

    /// Row-major unravel of a flat frame index into per-axis coordinates over
    /// `dims` (outermost first; the innermost axis varies fastest). This is C's
    /// extra-dimension odometer — `NDFileHDF5Dataset::extendDataSet`
    /// (NDFileHDF5Dataset.cpp:137-157) increments the innermost axis first and
    /// carries outward.
    fn unravel(mut idx: usize, dims: &[usize]) -> Vec<usize> {
        let mut coords = vec![0usize; dims.len()];
        for d in (0..dims.len()).rev() {
            let s = dims[d].max(1);
            coords[d] = idx % s;
            idx /= s;
        }
        coords
    }

    /// C++ `NDFileHDF5` chunk-size rule: 0, auto, or a value larger than the
    /// dimension means "chunk the whole dimension"; otherwise the user value.
    fn clamp_chunk(requested: usize, dim: usize, auto: bool) -> usize {
        if auto || requested == 0 || requested > dim {
            dim
        } else {
            requested
        }
    }

    /// Write one chunk band of `fc` consecutive frames into the primary
    /// dataset at the given `leading_coords` (the chunk-grid coordinates of the
    /// leading axes, outermost-first).
    ///
    /// For the extensible single-axis layout `leading_coords` is `[band_idx]`
    /// and up to `fc = chunk[0]` frames stack along that axis. Otherwise
    /// `leading_coords` is the frame's full position and every leading chunk
    /// is 1, so one frame is written per call; a Single file has none.
    ///
    /// The frame is split into `ceil(Y/rc) x ceil(X/cc)` chunk tiles, each
    /// written with `write_chunk_at(leading_coords ++ [row_tile, col_tile], ..)`.
    /// Tiles are `[fc, rc, cc]`; edge tiles and a partial final band (fewer
    /// than `fc` frames) are zero-padded; the frame axes' maximum is the
    /// frame size, so an edge tile does not extend them.
    fn flush_band(
        ds: &rust_hdf5::H5Dataset,
        leading_coords: &[usize],
        frames: &[Vec<u8>],
        frame_dims: &[usize],
        chunk: &[usize],
        elem_size: usize,
    ) -> ADResult<()> {
        for (coords, buf) in
            Self::band_chunk_writes(leading_coords, frames, frame_dims, chunk, elem_size)
        {
            ds.write_chunk_at(&coords, &buf).map_err(|e| {
                ADError::UnsupportedConversion(format!("HDF5 write_chunk_at error: {}", e))
            })?;
        }
        Ok(())
    }

    /// Write one frame at its leading `coords` by read-modify-write of the
    /// chunks it shares. The LE frame bytes go in as unsigned words of the
    /// stored element size; the dataset keeps its own datatype.
    fn write_frame_slice(
        ds: &rust_hdf5::H5Dataset,
        coords: &[usize],
        frame_dims: &[usize],
        frame_le: &[u8],
        elem_size: usize,
    ) -> ADResult<()> {
        let starts: Vec<usize> = coords
            .iter()
            .copied()
            .chain(frame_dims.iter().map(|_| 0))
            .collect();
        let counts: Vec<usize> = coords
            .iter()
            .map(|_| 1)
            .chain(frame_dims.iter().copied())
            .collect();
        macro_rules! slice {
            ($t:ty) => {{
                let words: Vec<$t> = frame_le
                    .chunks_exact(std::mem::size_of::<$t>())
                    .map(|b| <$t>::from_le_bytes(b.try_into().expect("exact chunk")))
                    .collect();
                ds.write_slice(&starts, &counts, &words)
            }};
        }
        let written = match elem_size {
            1 => slice!(u8),
            2 => slice!(u16),
            4 => slice!(u32),
            8 => slice!(u64),
            n => {
                return Err(ADError::UnsupportedConversion(format!(
                    "HDF5 positioned write of {n}-byte elements"
                )));
            }
        };
        written.map_err(|e| ADError::UnsupportedConversion(format!("HDF5 write_slice error: {e}")))
    }

    /// Frames one chunk holds: the innermost leading axis's chunk, or one
    /// frame when the dataset is the frame alone (a Single file).
    fn frames_per_chunk(chunk: &[usize], lead: usize) -> usize {
        lead.checked_sub(1).map_or(1, |i| chunk[i])
    }

    /// Compute the `(chunk_coords, chunk_bytes)` writes for one band of `frames`
    /// at `leading_coords`. The single source of tile math shared by the
    /// standard path's [`flush_band`](Self::flush_band) (writes each via
    /// `H5Dataset::write_chunk_at`) and the SWMR grid path
    /// ([`write_swmr`](Self::write_swmr), via `SwmrFileWriter::write_chunk_at`).
    ///
    /// `chunk` is the full per-chunk shape `[leading_chunk…, frame_chunk…]`;
    /// `fc`, the innermost leading chunk, frames stack along that axis (one
    /// frame when there is no leading axis). A 2-D frame is split into
    /// `ceil(Y/rc) x ceil(X/cc)` tiles `[fc, rc, cc]`; a non-2-D frame is one
    /// chunk per band. Edge tiles and a partial final band (fewer than `fc`
    /// frames) are zero-padded. Coords are in chunk units, row-major:
    /// `leading_coords ++ [row_tile, col_tile]` (or `++ [0; frame_rank]`).
    fn band_chunk_writes(
        leading_coords: &[usize],
        frames: &[Vec<u8>],
        frame_dims: &[usize],
        chunk: &[usize],
        elem_size: usize,
    ) -> Vec<(Vec<usize>, Vec<u8>)> {
        let lead = leading_coords.len();
        let fc = Self::frames_per_chunk(chunk, lead);
        // Non-2-D frame: one chunk per band, frames stacked along the first
        // leading axis (a partial band leaves trailing frames zero).
        if frame_dims.len() != 2 {
            let frame_len = frame_dims.iter().product::<usize>() * elem_size;
            let mut buf = vec![0u8; fc * frame_len];
            for (f, fb) in frames.iter().take(fc).enumerate() {
                buf[f * frame_len..f * frame_len + frame_len].copy_from_slice(fb);
            }
            let mut coords = leading_coords.to_vec();
            coords.extend(std::iter::repeat_n(0, frame_dims.len()));
            return vec![(coords, buf)];
        }

        let (y, x) = (frame_dims[0], frame_dims[1]);
        let (rc, cc) = (chunk[lead], chunk[lead + 1]);
        let row_tiles = y.div_ceil(rc);
        let col_tiles = x.div_ceil(cc);
        let mut writes = Vec::with_capacity(row_tiles * col_tiles);
        for ry in 0..row_tiles {
            for cx in 0..col_tiles {
                let mut tile = vec![0u8; fc * rc * cc * elem_size];
                for f in 0..fc {
                    let Some(fb) = frames.get(f) else {
                        break; // partial band: trailing frames stay zero
                    };
                    for r in 0..rc {
                        let sy = ry * rc + r;
                        if sy >= y {
                            break;
                        }
                        for c in 0..cc {
                            let sx = cx * cc + c;
                            if sx >= x {
                                break;
                            }
                            let src = (sy * x + sx) * elem_size;
                            let dst = ((f * rc + r) * cc + c) * elem_size;
                            tile[dst..dst + elem_size].copy_from_slice(&fb[src..src + elem_size]);
                        }
                    }
                }
                let mut coords = leading_coords.to_vec();
                coords.push(ry);
                coords.push(cx);
                writes.push((coords, tile));
            }
        }
        writes
    }

    /// Flush each detector dataset's partial frame band and trim its logical
    /// extent to its own frame count. Called from `close_file`. Every dataset in
    /// `detDataMap` is finalised independently, each at the count of frames the
    /// `detector_data_destination` routing sent it (C closes every
    /// `NDFileHDF5Dataset` it created, not just the default).
    fn finalize_standard_datasets(&mut self) -> ADResult<()> {
        let Some(frame_dims) = self.open_frame_dims.clone() else {
            return Ok(());
        };
        let (_, chunk, leading) = self.standard_layout(&frame_dims);
        let elem_size = self.stored_elem_size;
        let fc = Self::frames_per_chunk(&chunk, chunk.len() - frame_dims.len());
        let Some(Hdf5Handle::Standard { detectors, .. }) = self.handle.as_mut() else {
            return Ok(());
        };
        for det in detectors.values_mut() {
            let total = det.frame_count;
            // A partial band only survives the extensible single-axis layout
            // (the fixed multi-dim layout writes one frame per call, `fc == 1`).
            if !det.frame_band.is_empty() {
                let last = total.saturating_sub(1);
                let leading_coords = match &leading {
                    Some(lead) => Self::unravel(last, lead),
                    None => vec![last / fc],
                };
                Self::flush_band(
                    &det.ds,
                    &leading_coords,
                    &det.frame_band,
                    &frame_dims,
                    &chunk,
                    elem_size,
                )?;
                det.frame_band.clear();
            }
            // The extent C's `dims_` reached: write_chunk_at rounds the frame
            // axis up to the chunk, and a fixed-maximum index does not grow at
            // all.
            let mut dims = det.extent.clone();
            dims.extend_from_slice(&frame_dims);
            det.ds.set_extent(&dims).map_err(|e| {
                ADError::UnsupportedConversion(format!("HDF5 set_extent error: {}", e))
            })?;
        }
        Ok(())
    }

    /// Open file in SWMR streaming mode.
    ///
    /// Ordering mirrors C `NDFileHDF5::openFile` (`NDFileHDF5.cpp:173`-`337`):
    /// the file layout tree and datasets are created, then `createHardLinks`
    /// (`NDFileHDF5.cpp:319`-`320`) runs, and only then `startSWMR`
    /// (`NDFileHDF5.cpp:323`-`325`). `SwmrFileWriter` exposes `create_group` /
    /// `create_hard_link` callable before `start_swmr()`; a group or link
    /// created before `start_swmr()` is visible to SWMR readers for the whole
    /// streaming window. So here the image dataset is placed at the layout's
    /// nested `resolved_dataset_path` and the layout `<hardlink>` elements are
    /// materialised before SWMR mode is entered — not on the close path.
    ///
    /// The layout groups are built first because the dataset carries its full
    /// nested path as its name, and rust-hdf5 resolves that path against the
    /// groups that already exist (libhdf5's rule: the default link creation
    /// property list has `H5Pset_create_intermediate_group` off). The parent
    /// group the path names is therefore also the dataset's single placement
    /// owner — nothing re-parents it afterwards.
    fn open_swmr(&mut self, path: &Path, array: &NDArray) -> ADResult<()> {
        self.configure_compression(array);
        let mut swmr = SwmrFileWriter::create(path).map_err(|e| Self::create_failed(path, e))?;
        self.build_swmr_layout_groups(&mut swmr)?;
        if let Some(layout) = self.layout.as_ref() {
            Self::layout_detector_paths(layout)?;
        }
        if let Some(path) = self.hostname_group() {
            let group_path = format!("/{path}");
            let _ = self
                .created_attrs
                .on(path, SwmrGroup(&mut swmr, &group_path))
                .string("hostname", &self.hostname);
        }

        let usize_frame_dims: Vec<usize> = array.dims.iter().rev().map(|d| d.size).collect();
        let frame_dims: Vec<u64> = usize_frame_dims.iter().map(|&d| d as u64).collect();
        // A pre-compressed first frame gives the dataset its original type,
        // as on the standard path.
        let ds_data_type = array
            .codec
            .as_ref()
            .map_or(array.data().data_type(), |c| c.original_data_type);

        // Full chunk geometry, `[fc, rc, cc]`: HDF5_nFramesChunks deep and
        // the row/column tile sizes. rust-hdf5 0.2.15
        // `create_streaming_dataset_chunked` band-buffers whole frames and
        // zero-pads the final partial band at close, keeping the logical
        // frame count exact.
        // The standard path's dataspace (C's, compressed or not): a Single
        // file's frame alone, or with `HDF5_nExtraDims >= 1` a fixed grid of
        // shape `[eds[N], …, eds[0], Y, X]` filled at odometer positions via
        // `write_chunk_at`, otherwise one extensible frame axis.
        // C defect, not copied: C's SWMR Single file closes with its frame
        // unwritten, so `data` reads all fill (C oracle: storage 0). The
        // frame is written here as in a standard Single file.
        let (grid_shape_usize, grid_chunk_usize, mut grid_leading) =
            self.standard_layout(&usize_frame_dims);
        let max_shape: Vec<Option<u64>> = self
            .detector_max_shape(&grid_shape_usize, grid_leading.is_none())
            .into_iter()
            .map(|m| m.map(|v| v as u64))
            .collect();
        // rust-hdf5 appends to a SWMR frame axis in order only; positioned
        // frames are placed in a grid over their positions.
        if grid_leading.is_none() && self.cfg.pos_running {
            grid_leading = Some(self.virtual_dims());
        }
        let use_grid = grid_leading.is_some();
        let grid_chunk: Vec<u64> = grid_chunk_usize.iter().map(|&v| v as u64).collect();
        // C `dims_` starts at 1 on each leading axis.
        let lead_rank = grid_shape_usize.len() - usize_frame_dims.len();
        let grid_dims: Vec<u64> = std::iter::repeat_n(1, lead_rank)
            .chain(frame_dims.iter().copied())
            .collect();
        // The nbit filter travels with its datatype, as on the standard path.
        let (datatype, pipeline) = match self
            .cfg
            .nbit_packing(ds_data_type, grid_chunk_usize.iter().product())
        {
            Some((dt, pl)) => (dt, Some(pl)),
            None => (
                native_datatype(ds_data_type),
                self.cfg.build_pipeline(ds_data_type.element_size(), array),
            ),
        };
        let element_size = datatype.element_size() as usize;

        // The streaming dataset is created with its full nested layout path
        // as the dataset name (default flat `data` without a layout). The
        // `SwmrFileWriter` links it into the group that path names — built
        // just above — under just the leaf, while keeping the full name
        // addressable so a layout `<hardlink target="/entry/.../data">`
        // resolves against it.
        let ds_name = self.resolved_dataset_path.clone();

        let created = if use_grid {
            swmr.create_grid_dataset_with_max(
                &ds_name,
                datatype,
                &grid_dims,
                &max_shape,
                &grid_chunk,
                pipeline.clone(),
            )
        } else {
            swmr.create_streaming_dataset_with_max_frames(
                &ds_name,
                datatype,
                &frame_dims,
                &grid_chunk,
                max_shape[0],
                pipeline.clone(),
            )
        };
        let ds_index = created.map_err(|e| {
            ADError::UnsupportedConversion(format!("SWMR create dataset error: {}", e))
        })?;
        // C's SWMR dataset shares the dcpl H5Pset_fill_value set
        // (NDFileHDF5.cpp:3895).
        swmr.set_dataset_fill_value_bytes(
            ds_index,
            &stored_fill(self.cfg.fill_value, ds_data_type, element_size),
        )
        .map_err(|e| ADError::UnsupportedConversion(format!("SWMR fill value error: {}", e)))?;
        // Materialise the image dataset's constant attributes and the layout
        // `<hardlink>` elements — all BEFORE `start_swmr()` so SWMR readers
        // see the nested paths and aliases for the whole streaming window.
        // C `NDFileHDF5.cpp:319`-`325`: `createHardLinks` then `startSWMR`.
        self.write_swmr_layout_dataset_attrs(&mut swmr, ds_index)?;
        self.write_swmr_ndarray_default_attrs(&mut swmr, ds_index, array)?;
        self.create_swmr_attribute_datasets(&mut swmr)?;
        write_attribute_datasets(
            &mut self.attr_datasets,
            &self.file_attributes,
            LayoutWhen::OnFileOpen,
            None,
            Some(&mut swmr),
        )?;
        let perf = self.create_swmr_performance_dataset(&mut swmr);
        self.build_swmr_layout_hardlinks(&mut swmr)?;
        self.write_swmr_ndattr_element_attrs(&mut swmr, false)?;

        swmr.start_swmr()
            .map_err(|e| ADError::UnsupportedConversion(format!("SWMR start error: {}", e)))?;

        // Compression is applied to SWMR datasets via the filter pipeline
        // above. `compression_dropped` is only set when a compression type was
        // requested but no pipeline could be built for it (an unsupported
        // compressor) — never a silent drop.
        let compression_dropped = self.cfg.compression_type != COMPRESS_NONE && pipeline.is_none();
        if compression_dropped {
            epics_base_rs::stderr_println!(
                "NDFileHDF5: WARNING — SWMR mode requested compression type {} \
                 but no filter pipeline could be built for it; the SWMR file \
                 will be written UNCOMPRESSED.",
                self.cfg.compression_type
            );
        }

        let grid = if use_grid {
            // `use_grid` implies `grid_leading.is_some()`.
            grid_leading.map(|leading| SwmrGridLayout {
                extent: vec![1; leading.len()],
                leading,
                frame_dims: usize_frame_dims.clone(),
                chunk: grid_chunk_usize.clone(),
                elem_size: element_size,
            })
        } else {
            None
        };
        self.handle = Some(Hdf5Handle::Swmr {
            writer: Box::new(swmr),
            ds_index,
            compression_dropped,
            grid,
            perf,
        });
        self.open_data_type = Some(ds_data_type);
        self.stored_elem_size = element_size;
        self.open_frame_dims = Some(usize_frame_dims);
        self.dataset_codec = self.cfg.dataset_codec(ds_data_type, array);
        self.open_chunk = grid_chunk_usize;
        Ok(())
    }

    /// Build every group node declared in the loaded layout XML against a
    /// `SwmrFileWriter`, the SWMR counterpart of `build_layout_groups`.
    ///
    /// Paths are created parent-first (shortest path-depth first) via the
    /// rust-hdf5 0.2.17 `SwmrFileWriter::create_group` API, which takes the
    /// parent group path and a leaf name. Called from `open_swmr` before
    /// `start_swmr()` so the groups are visible to SWMR readers for the whole
    /// streaming window. No-op when no layout is loaded.
    fn build_swmr_layout_groups(&self, swmr: &mut SwmrFileWriter) -> ADResult<()> {
        let layout = match self.layout.as_ref() {
            Some(l) => l,
            None => return Ok(()),
        };
        post_unknown_types(
            &self.open_posts,
            layout.tree_attributes(self.cfg.store_attributes),
        );
        fn collect<'a>(
            g: &'a crate::hdf5_layout::LayoutGroup,
            prefix: &str,
            out: &mut Vec<(String, &'a crate::hdf5_layout::LayoutGroup)>,
        ) {
            let here = if prefix.is_empty() {
                g.name.clone()
            } else {
                format!("{}/{}", prefix, g.name)
            };
            out.push((here.clone(), g));
            for sub in &g.groups {
                collect(sub, &here, out);
            }
        }
        let mut nodes = Vec::new();
        for g in &layout.groups {
            collect(g, "", &mut nodes);
        }
        nodes.sort_by_key(|(p, _)| p.matches('/').count());
        let mut created: std::collections::HashSet<String> = std::collections::HashSet::new();
        for (path, node) in &nodes {
            if !created.insert(path.clone()) {
                continue;
            }
            let (parent, leaf) = match path.rsplit_once('/') {
                Some((p, l)) => (format!("/{}", p), l),
                None => ("/".to_string(), path.as_str()),
            };
            swmr.create_group(&parent, leaf).map_err(|e| {
                ADError::UnsupportedConversion(format!("SWMR layout group '{}': {}", path, e))
            })?;
            for d in &node.datasets {
                if d.source == crate::hdf5_layout::LayoutSource::Constant {
                    let _ = write_swmr_const_dataset(
                        swmr,
                        &format!("{}/{}", path, d.name),
                        d,
                        &self.created_attrs,
                    );
                }
            }
            // C attaches the group's constant <attribute> nodes (NX_class) via
            // writeHdfAttributes, under StoreAttr (NDFileHDF5.cpp:690-696); the
            // absolute group path is `/<path>`.
            if self.cfg.store_attributes {
                let group_path = format!("/{}", path);
                let mut target = self.created_attrs.on(path, SwmrGroup(swmr, &group_path));
                write_swmr_group_constant_attrs(&mut target, &group_path, &node.attributes)?;
            }
        }
        Ok(())
    }

    /// Materialise every `<hardlink>` declared in the loaded layout XML against
    /// a `SwmrFileWriter`, the SWMR counterpart of `build_layout_hardlinks`.
    ///
    /// Uses the rust-hdf5 0.2.17 `SwmrFileWriter::create_hard_link` API. Called
    /// from `open_swmr` after the layout groups and image dataset exist and
    /// before `start_swmr()` — matching C `NDFileHDF5.cpp:319`-`320`
    /// `createHardLinks`, which runs before `startSWMR`. A link created before
    /// `start_swmr()` is visible to SWMR readers for the whole streaming
    /// window. No-op when no layout is loaded.
    fn build_swmr_layout_hardlinks(&self, swmr: &mut SwmrFileWriter) -> ADResult<()> {
        let layout = match self.layout.as_ref() {
            Some(l) => l,
            None => return Ok(()),
        };
        fn collect<'a>(
            g: &'a crate::hdf5_layout::LayoutGroup,
            prefix: &str,
            out: &mut Vec<(String, &'a crate::hdf5_layout::LayoutHardlink)>,
        ) {
            let here = if prefix.is_empty() {
                g.name.clone()
            } else {
                format!("{}/{}", prefix, g.name)
            };
            for hl in &g.hardlinks {
                out.push((here.clone(), hl));
            }
            for sub in &g.groups {
                collect(sub, &here, out);
            }
        }
        let mut links = Vec::new();
        for g in &layout.groups {
            collect(g, "", &mut links);
        }
        for (parent_path, hl) in &links {
            let parent = format!("/{}", parent_path);
            swmr.create_hard_link(&parent, &hl.name, &hl.target)
                .map_err(|e| {
                    ADError::UnsupportedConversion(format!(
                        "SWMR layout hardlink '{}/{}' -> '{}': {}",
                        parent_path, hl.name, hl.target, e
                    ))
                })?;
        }
        Ok(())
    }

    /// The `<attribute>` nodes the layout attaches to the dataset whose full
    /// name is `full` (leading slash optional).
    fn layout_dataset_attrs(&self, full: &str) -> Vec<crate::hdf5_layout::LayoutAttribute> {
        let full = full.trim_start_matches('/');
        let mut attrs = Vec::new();
        if let Some(layout) = self.layout.as_ref() {
            layout.for_each_dataset(|path, d| {
                if format!("{}/{}", path, d.name).trim_start_matches('/') == full {
                    attrs.extend(d.attributes.iter().cloned());
                }
            });
        }
        attrs
    }

    /// Materialise the loaded layout XML's `constant` HDF5 attributes attached
    /// to the primary image dataset against a `SwmrFileWriter` (e.g. the NeXus
    /// `signal=1` marker); `ndattribute`-sourced nodes carry per-frame values
    /// and are written by [`Self::write_swmr_ndattr_element_attrs`].
    fn write_swmr_layout_dataset_attrs(
        &self,
        swmr: &mut SwmrFileWriter,
        ds_index: usize,
    ) -> ADResult<()> {
        let attrs = self.layout_dataset_attrs(&self.resolved_dataset_path);
        let mut target = self
            .created_attrs
            .on(&self.resolved_dataset_path, SwmrDataset(swmr, ds_index));
        for a in attrs
            .iter()
            .filter(|a| a.source == crate::hdf5_layout::LayoutSource::Constant)
        {
            target.layout_constant(a).map_err(|e| {
                ADError::UnsupportedConversion(format!(
                    "SWMR layout dataset attribute '{}': {}",
                    a.name, e
                ))
            })?;
        }
        Ok(())
    }

    /// Resolve the on-disk dataset/group paths from the loaded layout XML.
    ///
    /// With a valid layout this places the image dataset at the layout's
    /// `det_default` dataset path and the NDAttribute and performance datasets
    /// in C's groups (`Hdf5Layout::ndattr_home`, `performance_group`).
    ///
    /// All returned paths have the leading `/` stripped, since `rust-hdf5`
    /// keys datasets/groups without a leading slash.
    fn resolve_layout_paths(&mut self) {
        let strip = |s: String| s.trim_start_matches('/').to_string();
        match self.layout.as_ref() {
            Some(layout) => {
                self.resolved_dataset_path = layout
                    .detector_dataset_path()
                    .map(strip)
                    .unwrap_or_else(|| self.dataset_name.clone());
                self.resolved_ndattr_group = layout.ndattr_home().map(strip);
                self.resolved_perf_group = layout.performance_group().map(strip);
            }
            None => {
                self.resolved_dataset_path = self.dataset_name.clone();
                self.resolved_ndattr_group = None;
                self.resolved_perf_group = None;
            }
        }
    }

    /// Build every group node declared in the loaded layout XML so that empty
    /// NeXus-style groups (e.g. an `NXdata` placeholder) also exist on disk,
    /// not just the groups implied by the dataset placement. No-op when no
    /// layout is loaded.
    ///
    /// `rust-hdf5` 0.2.15's `create_group` errors on a duplicate path, so each
    /// distinct group path is created exactly once via a created-set; paths
    /// are processed shortest-first so a parent always exists before a child.
    fn build_layout_groups(&self, file: &H5File) -> ADResult<()> {
        let layout = match self.layout.as_ref() {
            Some(l) => l,
            None => return Ok(()),
        };
        post_unknown_types(
            &self.open_posts,
            layout.tree_attributes(self.cfg.store_attributes),
        );
        fn collect<'a>(
            g: &'a crate::hdf5_layout::LayoutGroup,
            prefix: &str,
            out: &mut Vec<(String, &'a crate::hdf5_layout::LayoutGroup)>,
        ) {
            let here = if prefix.is_empty() {
                g.name.clone()
            } else {
                format!("{}/{}", prefix, g.name)
            };
            out.push((here.clone(), g));
            for sub in &g.groups {
                collect(sub, &here, out);
            }
        }
        let mut nodes = Vec::new();
        for g in &layout.groups {
            collect(g, "", &mut nodes);
        }
        nodes.sort_by_key(|(p, _)| p.matches('/').count());
        let mut created: std::collections::HashSet<String> = std::collections::HashSet::new();
        for (path, node) in &nodes {
            if !created.insert(path.clone()) {
                continue;
            }
            let (parent, leaf) = match path.rsplit_once('/') {
                Some((p, l)) => (p, l),
                None => ("", path.as_str()),
            };
            // The parent path was created earlier (shorter, sorted first).
            let parent_group = if parent.is_empty() {
                None
            } else {
                Some(Self::open_write_group(file, parent)?)
            };
            let group = match parent_group.as_ref() {
                Some(g) => g.create_group(leaf),
                None => file.create_group(leaf),
            }
            .map_err(|e| {
                ADError::UnsupportedConversion(format!("HDF5 layout group '{}': {}", path, e))
            })?;
            // createTree writes the group's constant datasets, logging a failed
            // one and continuing (NDFileHDF5.cpp:664-688).
            for d in &node.datasets {
                if d.source == crate::hdf5_layout::LayoutSource::Constant {
                    let _ = write_const_dataset(&group, d, &self.created_attrs);
                }
            }
            // C attaches the group's constant <attribute> nodes (e.g. the NeXus
            // NX_class markers) via writeHdfAttributes, under StoreAttr
            // (NDFileHDF5.cpp:690-696).
            if self.cfg.store_attributes {
                let mut target = self.created_attrs.on(path, &group);
                write_group_constant_attrs(&mut target, &node.attributes);
            }
        }
        Ok(())
    }

    /// Materialise every `<hardlink>` declared in the loaded layout XML.
    ///
    /// A layout `<hardlink name="..." target="..."/>` inside a `<group>`
    /// declares an HDF5 hard link: an additional name (`name`, a leaf within
    /// the enclosing group) for the object already living at `target` (an
    /// absolute object path). C++ `NDFileHDF5::createHardLinks` walks the
    /// layout after the groups/datasets exist and calls `H5Lcreate_hard`.
    ///
    /// Called from `close_file` for the standard (non-SWMR) close path so that
    /// both the primary image dataset and the per-frame NDAttribute datasets —
    /// any of which a hardlink may target — already exist on disk. No-op when
    /// no layout is loaded.
    ///
    /// `file` is the live `Standard` write-mode HDF5 handle. The SWMR path has
    /// its own counterpart, `build_swmr_layout_hardlinks`, which runs before
    /// `start_swmr()` (C++ `NDFileHDF5.cpp:319`-`325`: `createHardLinks` then
    /// `startSWMR`) so SWMR readers see the links during streaming.
    fn build_layout_hardlinks(&self, file: &H5File) -> ADResult<()> {
        let layout = match self.layout.as_ref() {
            Some(l) => l,
            None => return Ok(()),
        };
        // Collect (parent_group_path, hardlink) for every group in the tree.
        fn collect<'a>(
            g: &'a crate::hdf5_layout::LayoutGroup,
            prefix: &str,
            out: &mut Vec<(String, &'a crate::hdf5_layout::LayoutHardlink)>,
        ) {
            let here = if prefix.is_empty() {
                g.name.clone()
            } else {
                format!("{}/{}", prefix, g.name)
            };
            for hl in &g.hardlinks {
                out.push((here.clone(), hl));
            }
            for sub in &g.groups {
                collect(sub, &here, out);
            }
        }
        let mut links = Vec::new();
        for g in &layout.groups {
            collect(g, "", &mut links);
        }
        for (parent_path, hl) in &links {
            // The enclosing group already exists (created by
            // `build_layout_groups`); re-open it and create the link inside it.
            let parent = Self::open_write_group(file, parent_path)?;
            parent.link(&hl.name, &hl.target).map_err(|e| {
                ADError::UnsupportedConversion(format!(
                    "HDF5 layout hardlink '{}/{}' -> '{}': {}",
                    parent_path, hl.name, hl.target, e
                ))
            })?;
        }
        Ok(())
    }

    /// Re-open an already-created group by full path in write mode. In write
    /// mode `H5Group::group` returns a handle without verification, so this is
    /// a pure handle constructor walking each path segment.
    fn open_write_group(file: &H5File, path: &str) -> ADResult<rust_hdf5::H5Group> {
        let mut current: Option<rust_hdf5::H5Group> = None;
        for seg in path.split('/').filter(|s| !s.is_empty()) {
            let next = match current.as_ref() {
                Some(g) => g.group(seg),
                None => file.root_group().group(seg),
            }
            .map_err(|e| {
                ADError::UnsupportedConversion(format!("HDF5 group reopen '{}': {}", seg, e))
            })?;
            current = Some(next);
        }
        // An empty path is the root group.
        Ok(current.unwrap_or_else(|| file.root_group()))
    }

    /// Create every detector-source dataset at open in standard mode. Each is
    /// an extensible `[nframes, .., Y, X]` array whose leading dimension later
    /// frames extend (C++ `NDFileHDF5Dataset`). C creates one
    /// `NDFileHDF5Dataset` per `<dataset source="detector">` node up front
    /// (`createDatasetDetector`, NDFileHDF5.cpp:1324-1357) and routes frames
    /// among them; the common single-dataset layout produces exactly one. All
    /// datasets share the datatype/dataspace/frame shape of the open array.
    fn create_detector_datasets(&mut self, array: &NDArray) -> ADResult<()> {
        let frame_dims: Vec<usize> = array.dims.iter().rev().map(|d| d.size).collect();
        // A pre-compressed first frame gives the dataset its ORIGINAL element
        // type. The chunk is the user's as for any frame (NDFileHDF5.cpp:
        // 3234-3271), so a compressed frame that is not exactly one chunk is
        // refused in `write_frame` as `verifyChunking` refuses it. The filter is
        // the scheme's, after the frame's codec has overridden it in the params.
        self.configure_compression(array);
        let ds_data_type = array
            .codec
            .as_ref()
            .map_or(array.data().data_type(), |c| c.original_data_type);
        let (shape, chunk, leading) = self.standard_layout(&frame_dims);
        // N-bit packing couples a reduced-precision datatype override with the
        // nbit filter (C narrows `this->datatype` then `H5Pset_nbit`).
        // `d_nelmts` is the per-chunk element count.
        let chunk_nelmts: usize = chunk.iter().product();
        let (nbit_dt, pipeline) = match self.cfg.nbit_packing(ds_data_type, chunk_nelmts) {
            Some((dt, pl)) => (Some(dt), Some(pl)),
            None => (
                None,
                self.cfg.build_pipeline(ds_data_type.element_size(), array),
            ),
        };
        let stored_elem_size = nbit_dt
            .as_ref()
            .map_or(ds_data_type.element_size(), |dt| dt.element_size() as usize);
        let max_shape = self.detector_max_shape(&shape, leading.is_none());

        // Build the layout group hierarchy (if a layout XML is loaded) before
        // placing the dataset. With no layout this is a no-op and the dataset
        // lands flat at the file root.
        match self.handle {
            Some(Hdf5Handle::Standard { ref file, .. }) => self.build_layout_groups(file)?,
            _ => return Err(ADError::UnsupportedConversion("no HDF5 file open".into())),
        }

        // Enumerate every detector-source dataset (C `detDataMap`), each with
        // the `<attribute>` nodes the layout XML attaches to it (e.g. the
        // NeXus `signal=1` marker). Keys are leading-slash full names so the
        // `detector_data_destination` routing value matches C `detDataMap`.
        let detector_keys: Vec<String> = match self.layout.as_ref() {
            Some(l) => Self::layout_detector_paths(l)?,
            None => vec![format!("/{}", self.resolved_dataset_path)],
        };
        let detector_specs: Vec<(String, Vec<crate::hdf5_layout::LayoutAttribute>)> = detector_keys
            .iter()
            .map(|key| (key.clone(), self.layout_dataset_attrs(key)))
            .collect();

        let fill = self.cfg.fill_value;

        macro_rules! create_ds {
            ($t:ty, $h5file:expr, $ds_group:expr, $ds_name:expr) => {{
                let mut builder = match $ds_group.as_ref() {
                    Some(g) => g.new_dataset::<$t>(),
                    None => $h5file.new_dataset::<$t>(),
                }
                .shape(&shape[..])
                .chunk(&chunk[..])
                .max_shape(&max_shape[..]);
                // C parity: NDFileHDF5 sets HDF5_fillValue on the dataset
                // creation property list (H5Pset_fill_value), so unwritten
                // chunks read back as `fill` rather than zero. An nbit
                // datatype (C narrows `this->datatype`) takes the fill in its
                // stored width, widened as the data is.
                if let Some(ref dt) = nbit_dt {
                    builder = builder.datatype(dt.clone());
                }
                builder =
                    builder.fill_value_bytes(&stored_fill(fill, ds_data_type, stored_elem_size));
                if let Some(ref pl) = pipeline {
                    builder = builder.filter_pipeline(pl.clone());
                }
                builder.create($ds_name.as_str()).map_err(|e| {
                    ADError::UnsupportedConversion(format!("HDF5 dataset error: {}", e))
                })?
            }};
        }

        let h5file = match self.handle {
            Some(Hdf5Handle::Standard { ref file, .. }) => file,
            _ => return Err(ADError::UnsupportedConversion("no HDF5 file open".into())),
        };
        if let Some(path) = self.hostname_group()
            && let Ok(group) = Self::open_write_group(h5file, path)
        {
            let _ = self
                .created_attrs
                .on(path, &group)
                .string("hostname", &self.hostname);
        }

        let mut detectors: std::collections::HashMap<String, DetectorDataset> =
            std::collections::HashMap::with_capacity(detector_specs.len());
        for (key, layout_attrs) in &detector_specs {
            // Resolve each dataset's parent group and leaf name. A key is e.g.
            // `/entry/instrument/detector/data` with a layout, or `/data` flat.
            let stripped = key.trim_start_matches('/');
            let (ds_group, ds_name): (Option<rust_hdf5::H5Group>, String) =
                match stripped.rsplit_once('/') {
                    Some((group_path, leaf)) => (
                        Some(Self::open_write_group(h5file, group_path)?),
                        leaf.to_string(),
                    ),
                    None => (None, stripped.to_string()),
                };
            // Dispatch the dataset element type on `ds_data_type`: the original
            // (uncompressed) type for a pre-compressed array — whose `array.data`
            // is the collapsed `U8` byte buffer — and the buffer's own type
            // otherwise.
            let ds = match ds_data_type {
                NDDataType::Int8 => create_ds!(i8, h5file, ds_group, ds_name),
                NDDataType::UInt8 => create_ds!(u8, h5file, ds_group, ds_name),
                NDDataType::Int16 => create_ds!(i16, h5file, ds_group, ds_name),
                NDDataType::UInt16 => create_ds!(u16, h5file, ds_group, ds_name),
                NDDataType::Int32 => create_ds!(i32, h5file, ds_group, ds_name),
                NDDataType::UInt32 => create_ds!(u32, h5file, ds_group, ds_name),
                NDDataType::Int64 => create_ds!(i64, h5file, ds_group, ds_name),
                NDDataType::UInt64 => create_ds!(u64, h5file, ds_group, ds_name),
                NDDataType::Float32 => create_ds!(f32, h5file, ds_group, ds_name),
                NDDataType::Float64 => create_ds!(f64, h5file, ds_group, ds_name),
            };
            // Materialise the layout XML's constant dataset attributes (e.g.
            // NeXus `signal=1`), then the NDArray geometry.
            let mut target = self.created_attrs.on(key, &ds);
            for a in layout_attrs
                .iter()
                .filter(|a| a.source == crate::hdf5_layout::LayoutSource::Constant)
            {
                let _ = target.layout_constant(a);
            }
            for (name, vals) in ndarray_default_attrs(array, layout_attrs) {
                let _ = target.numbers(name, &vals);
            }
            detectors.insert(
                key.clone(),
                DetectorDataset {
                    ds,
                    frame_count: 0,
                    frame_band: Vec::new(),
                    extent: vec![1; leading.as_ref().map_or(1, Vec::len)],
                },
            );
        }

        if let Some(Hdf5Handle::Standard { detectors: d, .. }) = self.handle.as_mut() {
            *d = detectors;
        }
        self.open_data_type = Some(ds_data_type);
        self.stored_elem_size = stored_elem_size;
        self.open_frame_dims = Some(frame_dims);
        self.dataset_codec = self.cfg.dataset_codec(ds_data_type, array);
        self.open_chunk = chunk;
        Ok(())
    }

    /// Resolve which detector dataset a frame is written to (C
    /// `NDFileHDF5::writeFile`, NDFileHDF5.cpp:1449-1474). The default is
    /// `defDsetName` — the leading-slash full name of the `det_default`/first
    /// detector dataset (= `resolved_dataset_path`). When the layout's
    /// `<global name="detector_data_destination" ndattribute="X"/>` names an
    /// NDAttribute that the frame carries as a *string* whose value is an
    /// existing detector-dataset key, that key is the destination; an unknown
    /// value falls back to the default. A present-but-non-string attribute is
    /// an error, matching C (`getValue(NDAttrString,…)` → `ND_ERROR`, which
    /// aborts the write).
    fn resolve_destination_key(&self, array: &NDArray) -> ADResult<String> {
        let default = format!("/{}", self.resolved_dataset_path);
        let attr_name = self
            .layout
            .as_ref()
            .and_then(|l| l.detector_data_destination.as_deref())
            .filter(|s| !s.is_empty());
        let Some(attr_name) = attr_name else {
            return Ok(default);
        };
        // C 1451-1452: a missing destination attribute keeps the default with
        // no error (only a present-but-unreadable one aborts).
        let Some(attr) = array.attributes.get(attr_name) else {
            return Ok(default);
        };
        match attr.value.as_string_typed() {
            Some(value) => {
                let known = matches!(
                    &self.handle,
                    Some(Hdf5Handle::Standard { detectors, .. })
                        if detectors.contains_key(value)
                );
                Ok(if known { value.to_string() } else { default })
            }
            None => Err(ADError::UnsupportedConversion(format!(
                "HDF5 detector_data_destination attribute '{}' is not a string",
                attr_name
            ))),
        }
    }

    /// Write a frame in standard (non-SWMR) mode, routing it to its detector
    /// dataset (C `detector_data_destination`) and extending that dataset's
    /// leading dimension. Each detector dataset keeps its own frame counter and
    /// band; the file-wide `frame_count` is the total across all of them.
    fn write_standard(&mut self, array: &NDArray) -> Result<(), FrameError> {
        let frame_dims = self
            .open_frame_dims
            .clone()
            .ok_or_else(|| ADError::UnsupportedConversion("dataset not initialised".into()))?;
        let cur_dims: Vec<usize> = array.dims.iter().rev().map(|d| d.size).collect();
        if cur_dims != frame_dims {
            return Err(ADError::UnsupportedConversion(format!(
                "HDF5 frame shape changed mid-stream: {:?} != {:?}",
                cur_dims, frame_dims
            ))
            .into());
        }

        // C resolves the destination first; its error returns without the
        // abort (NDFileHDF5.cpp:1467-1473).
        let dest_key = self.resolve_destination_key(array)?;
        let elsewhere = dest_key != format!("/{}", self.resolved_dataset_path);
        // Every frame needs its positions; only the default dataset's go
        // there (NDFileHDF5.cpp:1480-1517).
        let offsets = self.frame_positions(array).map_err(FrameError::Dataset)?;
        let positions = offsets.clone().filter(|_| self.multi_frame && !elsewhere);
        if let Some(offsets) = &positions {
            let leaf = dest_key.rsplit('/').next().unwrap_or_default();
            self.check_positions(offsets, leaf)
                .map_err(FrameError::Dataset)?;
        }
        let chunk = self.open_chunk.clone();
        let chunk_bytes = self.direct_chunk(array, &frame_dims)?;

        let (_shape, _, leading) = self.standard_layout(&frame_dims);
        let fc = Self::frames_per_chunk(&chunk, chunk.len() - frame_dims.len());

        // An uncompressed frame becomes little-endian pixel bytes for band
        // tiling, before the detector map is borrowed mutably.
        let elem_size = self.stored_elem_size;
        let frame_le = if chunk_bytes.is_none() {
            Some(widen_le(
                nd_buffer_to_le_bytes(array.data()),
                array.data().data_type(),
                elem_size,
            ))
        } else {
            None
        };

        {
            let Some(Hdf5Handle::Standard { detectors, .. }) = self.handle.as_mut() else {
                return Err(ADError::UnsupportedConversion(
                    "HDF5 detector datasets not initialised".into(),
                )
                .into());
            };
            let det = detectors.get_mut(&dest_key).ok_or_else(|| {
                ADError::UnsupportedConversion(format!(
                    "HDF5 destination dataset '{}' not found",
                    dest_key
                ))
            })?;

            // The frame's leading-axis coordinates: its positions, or the
            // per-dataset odometer, which a fixed extra-dim layout bounds by
            // the product of the extra-dim sizes.
            let frame_idx = det.frame_count;
            // Frames in order along the one extensible axis go out a whole
            // `fc`-deep chunk at a time.
            let banded = leading.is_none() && positions.is_none();
            let coords = match (positions, &leading) {
                (Some(offsets), _) => offsets,
                (None, Some(lead)) => {
                    let total: usize = lead.iter().product();
                    if frame_idx >= total {
                        return Err(FrameError::Dataset(ADError::UnsupportedConversion(
                            format!(
                                "HDF5 extra-dimension capacity exceeded: frame {} >= {}",
                                frame_idx, total
                            ),
                        )));
                    }
                    Self::unravel(frame_idx, lead)
                }
                (None, None) => vec![frame_idx],
            };
            for (e, c) in det.extent.iter_mut().zip(&coords) {
                *e = (*e).max(c + 1);
            }

            if let Some(cb) = &chunk_bytes {
                // Direct chunk write. The frame passed the one-frame chunk check
                // above, so each frame is exactly one chunk at its coordinates.
                // filter_mask = 0: the codec already applied the full pipeline.
                let mut at = coords.clone();
                at.resize(chunk.len(), 0);
                det.ds.write_chunk_raw_at(&at, cb, 0).map_err(|e| {
                    FrameError::Dataset(ADError::UnsupportedConversion(format!(
                        "HDF5 direct chunk write error: {}",
                        e
                    )))
                })?;
            } else if !banded
                && chunk[..chunk.len() - frame_dims.len()]
                    .iter()
                    .any(|&c| c > 1)
            {
                // The frame shares its chunk with frames that come in any
                // order, so it is patched in, as C's hyperslab write does.
                let mut dims = det.extent.clone();
                dims.extend_from_slice(&frame_dims);
                det.ds.set_extent(&dims).map_err(|e| {
                    FrameError::Dataset(ADError::UnsupportedConversion(format!(
                        "HDF5 set_extent error: {}",
                        e
                    )))
                })?;
                let fle = frame_le.expect("frame_le is set whenever chunk_bytes is None");
                Self::write_frame_slice(&det.ds, &coords, &frame_dims, &fle, elem_size)
                    .map_err(FrameError::Dataset)?;
            } else {
                // Uncompressed: frames accumulate in this dataset's band and a
                // full `fc`-deep band is flushed as a grid of write_chunk_at
                // tiles; close_file flushes the partial final band. Unbanded,
                // every leading chunk is 1, so `fc == 1` and each frame is
                // placed at its coordinates.
                let fle = frame_le.expect("frame_le is set whenever chunk_bytes is None");
                det.frame_band.push(fle);
                if det.frame_band.len() >= fc {
                    let leading_coords = if banded { vec![frame_idx / fc] } else { coords };
                    Self::flush_band(
                        &det.ds,
                        &leading_coords,
                        &det.frame_band,
                        &frame_dims,
                        &chunk,
                        elem_size,
                    )
                    .map_err(FrameError::Dataset)?;
                    det.frame_band.clear();
                }
            }
            det.frame_count += 1;
        }

        // C `writeAttributeDataset(OnFrame)` follows the frame; a dataset that
        // follows the default dataset's dimensions skips a frame sent elsewhere
        // (NDFileHDF5.cpp:1560-1570).
        if self.cfg.store_attributes && !(self.cfg.dim_att_datasets && elsewhere) {
            write_attribute_datasets(
                &mut self.attr_datasets,
                &self.file_attributes,
                LayoutWhen::OnFrame,
                offsets.as_deref(),
                None,
            )
            .map_err(FrameError::Dataset)?;
        }
        Ok(())
    }

    /// Create one attribute time-series dataset per NDAttribute, preserving
    /// the NDAttrValue numeric type. Mirrors C++ `createAttributeDataset`.
    fn create_attribute_datasets(&mut self) {
        self.attr_datasets.clear();
        if !self.cfg.store_attributes {
            return;
        }
        let layout = self.layout.as_ref();
        let home = self.resolved_ndattr_group.as_deref();
        let shape = self.attribute_shape();
        let mut declared = Vec::new();
        // A dimAttDatasets dataset takes the frame's offsets on its axes.
        let axes: Vec<usize> = match &shape {
            _ if !self.cfg.dim_att_datasets => Vec::new(),
            AttrShape::Series { .. } => vec![0],
            AttrShape::Grid { dims, .. } => (0..dims.len()).collect(),
            AttrShape::Scalar => Vec::new(),
        };
        self.attr_datasets = self
            .file_attributes
            .iter()
            .filter_map(|attr| {
                // C `find_dset_ndattr` places a declared dataset; any other goes
                // to the default group, or nowhere without one
                // (NDFileHDF5.cpp:2792-2857).
                let (group, dset_name, when) =
                    match layout.and_then(|l| l.ndattribute_dataset(&attr.name)) {
                        Some((g, d)) => {
                            declared.extend(&d.attributes);
                            (
                                g.trim_start_matches('/').to_string(),
                                d.name.clone(),
                                d.when,
                            )
                        }
                        None => (home?.to_string(), attr.name.clone(), LayoutWhen::OnFrame),
                    };
                // C `createAttributeDataset`: in PositionMode an attribute a
                // `HDF5_posIndexDim*` names is a plain series of that axis's
                // values (NDFileHDF5.cpp:2803-2809).
                let (shape, axes) = match self.attribute_index(&attr.name) {
                    Some(i) if self.cfg.dim_att_datasets && self.cfg.pos_running => (
                        AttrShape::Series {
                            chunk: self.attribute_chunking(),
                        },
                        vec![i],
                    ),
                    _ => (shape.clone(), axes.clone()),
                };
                Some(AttributeDataset::new(
                    attr, group, dset_name, when, shape, axes,
                ))
            })
            .collect();
        // writeHdfAttributes on each declared dataset (:2816).
        post_unknown_types(&self.open_posts, declared);
        // C writes and flushes NDArrayUniqueId after every other attribute
        // (NDFileHDF5.cpp:355-390, :2955-2992), so a SWMR reader that sees a
        // frame's id sees the rest of it. Writes follow this list, and so does
        // rust-hdf5's flush, which publishes datasets in creation order.
        self.attr_datasets
            .sort_by_key(|ad| ad.name == "NDArrayUniqueId");
    }

    /// C `isAttributeIndex` (NDFileHDF5.cpp:3956-3971): the extra dimension,
    /// outermost first, whose `HDF5_posIndexDim*` names `name`. The last match
    /// wins; one past `nExtraDims` is none, where C goes on to read
    /// `offsets[-k]`.
    fn attribute_index(&self, name: &str) -> Option<usize> {
        let i = self.cfg.pos_index_names.iter().rposition(|n| n == name)?;
        self.cfg.n_extra_dims.checked_sub(i)
    }

    /// Effective chunk depth for the NDAttribute datasets and the performance
    /// dataset's leading dimension, mirroring C `calculateAttributeChunking`
    /// (NDFileHDF5.cpp:2869-2920). The `HDF5_NDAttributeChunk` param (0 = auto):
    /// when auto, Single mode chunks at 1; otherwise at the capture target
    /// (`NDFileNumCapture`), or `16*1024` when that is unlimited (≤ 0).
    fn attribute_chunking(&self) -> usize {
        if self.cfg.chunk.ndattr_chunk != 0 {
            return self.cfg.chunk.ndattr_chunk;
        }
        if !self.multi_frame {
            return 1;
        }
        if self.num_capture > 0 {
            self.num_capture as usize
        } else {
            16 * 1024
        }
    }

    /// C `createAttributeDataset`'s geometry (NDFileHDF5.cpp:2803-2813):
    /// `createDataset(chunking)`, or under `HDF5_dimAttDatasets` the frame's
    /// extra dimensions, chunked by `calculateAttributeChunking`'s
    /// per-dimension values (:2885-2915).
    fn attribute_shape(&self) -> AttrShape {
        if !self.cfg.dim_att_datasets {
            return AttrShape::Series {
                chunk: self.attribute_chunking(),
            };
        }
        if !self.multi_frame {
            return AttrShape::Scalar;
        }
        // C `mdim_chunking[k]`; an extra-dimension size here is at least 1.
        let chunk = |k: usize| match (self.cfg.chunk.ndattr_chunk, self.num_capture) {
            (0, 0) => 32,
            (0, _) => self.cfg.extra_dims[k].size.max(1),
            (c, _) => c,
        };
        match self.extra_dim_axes() {
            // C's rank-1 dataset grows as a series (NDFileHDF5AttributeDataset.cpp:385-389).
            None => AttrShape::Series { chunk: chunk(0) },
            Some(dims) => {
                let n = dims.len() - 1;
                AttrShape::Grid {
                    chunk: (0..=n).map(|i| chunk(n - i)).collect(),
                    dims,
                }
            }
        }
    }

    /// Reset and seed the NDAttribute element-attr value caches (ADP-79) from
    /// the open-time frame. The distinct referenced names are cached so the
    /// per-frame update (`update_ndattr_element_values`) need not re-walk the
    /// layout. No-op unless the layout declares `<attribute source="ndattribute">`
    /// on a group or dataset (the default NeXus layout declares none).
    ///
    /// C stores these only when StoreAttr is 1 (NDFileHDF5.cpp:297-309,
    /// :1661-1664); with it 0 the caches stay empty, so no element-attr has a
    /// value to write.
    fn seed_ndattr_element_values(&mut self) {
        self.ndattr_first_values.clear();
        self.ndattr_last_values.clear();
        self.ndattr_element_names.clear();
        let Some(layout) = self.layout.as_ref().filter(|_| self.cfg.store_attributes) else {
            return;
        };
        let mut names: Vec<String> = Vec::new();
        for e in layout.ndattribute_element_attrs() {
            if !names.contains(&e.ndattribute) {
                names.push(e.ndattribute);
            }
        }
        for name in &names {
            if let Some(attr) = self.file_attributes.get(name) {
                self.ndattr_first_values
                    .insert(name.clone(), attr.value.clone());
                self.ndattr_last_values
                    .insert(name.clone(), attr.value.clone());
            }
        }
        self.ndattr_element_names = names;
    }

    /// Update the last-frame value cache for every referenced element-attr
    /// NDAttribute name (C `pFileAttributes` tracks the most recent frame, so a
    /// `when="OnFileClose"` attribute records the final value). First-frame
    /// values are left untouched — an attribute absent at open stays unwritten
    /// for `OnFileOpen`/`OnFrame`, matching C's open-time `find` miss.
    fn update_ndattr_element_values(&mut self) {
        if self.ndattr_element_names.is_empty() {
            return;
        }
        let Self {
            ndattr_element_names,
            ndattr_last_values,
            file_attributes,
            ..
        } = self;
        for name in ndattr_element_names.iter() {
            if let Some(attr) = file_attributes.get(name) {
                ndattr_last_values.insert(name.clone(), attr.value.clone());
            }
        }
    }

    /// Materialise every layout `<attribute source="ndattribute">` as an HDF5
    /// attribute on its group/dataset (C `storeOnOpenCloseAttribute`). Called at
    /// close on the standard path, when every group and dataset exists: an
    /// `OnFileOpen`/`OnFrame` attribute takes the first-frame value (C writes it
    /// at open, where a later close re-create would fail as a duplicate, so the
    /// open value wins), an `OnFileClose` attribute the last-frame value.
    /// The HDF5 attribute datatype follows the live NDAttribute value (C `typeNd2Hdf` on
    /// the runtime type); an undefined/absent value is skipped.
    ///
    /// Group attributes are written through the file handle. Dataset attributes
    /// need a live dataset handle: a detector dataset retains its create-time
    /// handle in `detectors` at close, and the lazily-created attribute/
    /// performance datasets are reopened by full path through
    /// `H5File::dataset_writer` (rust-hdf5 0.2.22), so dataset element-attrs are
    /// honoured for every dataset.
    fn flush_ndattr_element_attrs(
        &self,
        file: &H5File,
        detectors: &std::collections::HashMap<String, DetectorDataset>,
    ) -> ADResult<()> {
        let Some(layout) = self.layout.as_ref() else {
            return Ok(());
        };
        for e in layout.ndattribute_element_attrs() {
            let Some(value) = self.ndattr_element_value(&e) else {
                continue;
            };
            let path = e.element_path.trim_start_matches('/');
            if e.is_dataset {
                // A detector dataset retains its create-time write handle in
                // `detectors` (keyed by leading-slash full name); the lazily-
                // created attribute/performance datasets do not. Both exist on
                // disk by now (close-path ordering), so reopen the latter by
                // full path in write mode (rust-hdf5 0.2.22 `dataset_writer`,
                // which registers names as slash-trimmed full paths).
                if let Some(det) = detectors.get(&e.element_path) {
                    let _ = write_ndattr_value(
                        &mut self.created_attrs.on(path, &det.ds),
                        &e.attr_name,
                        value,
                    );
                } else if let Ok(ds) = file.dataset_writer(path) {
                    let _ = write_ndattr_value(
                        &mut self.created_attrs.on(path, &ds),
                        &e.attr_name,
                        value,
                    );
                }
            } else {
                let group = Self::open_write_group(file, path)?;
                let _ = write_ndattr_value(
                    &mut self.created_attrs.on(path, &group),
                    &e.attr_name,
                    value,
                );
            }
        }
        Ok(())
    }

    /// [`ndarray_default_attrs`] on the single streaming dataset (addressed by
    /// `ds_index`), before `start_swmr()` as C writes them at open.
    fn write_swmr_ndarray_default_attrs(
        &self,
        swmr: &mut SwmrFileWriter,
        ds_index: usize,
        array: &NDArray,
    ) -> ADResult<()> {
        let layout_attrs = self.layout_dataset_attrs(&self.resolved_dataset_path);
        let mut target = self
            .created_attrs
            .on(&self.resolved_dataset_path, SwmrDataset(swmr, ds_index));
        for (name, vals) in ndarray_default_attrs(array, &layout_attrs) {
            target.numbers(name, &vals).map_err(|e| {
                ADError::UnsupportedConversion(format!("SWMR {} attr: {}", name, e))
            })?;
        }
        Ok(())
    }

    /// The value a layout `<attribute source="ndattribute">` is written with:
    /// `OnFileOpen`/`OnFrame` the first-frame value, `OnFileClose` the last.
    fn ndattr_element_value(&self, e: &NdAttrElementAttr) -> Option<&NDAttrValue> {
        match e.when {
            LayoutWhen::OnFileOpen | LayoutWhen::OnFrame => {
                self.ndattr_first_values.get(&e.ndattribute)
            }
            LayoutWhen::OnFileClose => self.ndattr_last_values.get(&e.ndattribute),
        }
    }

    /// SWMR counterpart of [`Hdf5Writer::flush_ndattr_element_attrs`], in C's
    /// two phases: before `start_swmr()` the `OnFileOpen`/`OnFrame` nodes
    /// (`storeOnOpenAttributes`), at close the `OnFileClose` ones
    /// (`storeOnCloseAttributes`). Group attrs address the group by path;
    /// dataset attrs address any dataset the open has created (the
    /// detector's and the layout constants) by its path.
    fn write_swmr_ndattr_element_attrs(
        &self,
        swmr: &mut SwmrFileWriter,
        at_close: bool,
    ) -> ADResult<()> {
        let Some(layout) = self.layout.as_ref() else {
            return Ok(());
        };
        for e in layout.ndattribute_element_attrs() {
            if (e.when == LayoutWhen::OnFileClose) != at_close {
                continue;
            }
            let Some(value) = self.ndattr_element_value(&e) else {
                continue;
            };
            // storeOnOpenCloseAttribute only warns on a failed attribute.
            let path = e.element_path.trim_start_matches('/');
            if e.is_dataset {
                if let Some(index) = swmr.dataset_index(path) {
                    let mut target = self.created_attrs.on(path, SwmrDataset(swmr, index));
                    let _ = write_ndattr_value(&mut target, &e.attr_name, value);
                }
            } else {
                let mut target = self
                    .created_attrs
                    .on(path, SwmrGroup(swmr, &e.element_path));
                let _ = write_ndattr_value(&mut target, &e.attr_name, value);
            }
        }
        Ok(())
    }

    /// C `createPerformanceDataset` (NDFileHDF5.cpp:2596-2671) on a
    /// `SwmrFileWriter`, before `start_swmr()`. `None` where C only warns and
    /// keeps no performance dataset (NDFileHDF5.cpp:313-316), as when an
    /// NDAttribute holds `timestamp`.
    fn create_swmr_performance_dataset(&self, swmr: &mut SwmrFileWriter) -> Option<usize> {
        let group = self
            .resolved_perf_group
            .as_deref()
            .filter(|_| self.cfg.store_performance)?;
        let path = if group.is_empty() {
            "timestamp".to_owned()
        } else {
            format!("{group}/timestamp")
        };
        let chunk = self.attribute_chunking() as u64;
        swmr.create_grid_dataset_with_max(
            &path,
            f64::hdf5_type(),
            &[1, 5],
            &[None, None],
            &[chunk, 5],
            None,
        )
        .ok()
    }

    /// C `createAttributeDataset` (NDFileHDF5.cpp:2707-2867) on a
    /// `SwmrFileWriter`, before `start_swmr()`: one dataset per NDAttribute
    /// with C's datatype, fill and chunk, its layout constants and
    /// descriptors. `write_swmr` writes the values.
    fn create_swmr_attribute_datasets(&mut self, swmr: &mut SwmrFileWriter) -> ADResult<()> {
        self.create_attribute_datasets();
        let err = |e: rust_hdf5::Hdf5Error| {
            ADError::UnsupportedConversion(format!("SWMR attribute dataset error: {e}"))
        };
        let bounds = self.virtual_dims();
        let positioned = self.cfg.pos_running;
        for ad in &mut self.attr_datasets {
            let path = ad.path();
            let fill = ad.fill();
            ad.shape = match std::mem::replace(&mut ad.shape, AttrShape::Scalar) {
                // A stream only appends, so a positioned series is a grid
                // over the positions of its axis.
                AttrShape::Series { chunk } if positioned && !ad.offset_axes.is_empty() => {
                    AttrShape::Grid {
                        dims: vec![bounds[ad.offset_axes[0]]],
                        chunk: vec![chunk],
                    }
                }
                shape => shape,
            };
            let to64 = |v: &[usize]| v.iter().map(|&x| x as u64).collect::<Vec<_>>();
            ad.sink = match &ad.shape {
                // C `dims_` 1 and `maxdims_` unlimited on every axis
                // (NDFileHDF5AttributeDataset.cpp:251-255, :298-304).
                AttrShape::Grid { dims, chunk } => {
                    let index = swmr
                        .create_grid_dataset_with_max(
                            &path,
                            ad.datatype(),
                            &vec![1; dims.len()],
                            &vec![None; dims.len()],
                            &to64(chunk),
                            None,
                        )
                        .map_err(err)?;
                    AttrSink::SwmrGrid {
                        index,
                        cells: grid_cells(&[], dims, &fill),
                        dirty: Default::default(),
                    }
                }
                AttrShape::Series { chunk } => AttrSink::Swmr(
                    swmr.create_streaming_dataset_with_max_frames(
                        &path,
                        ad.datatype(),
                        &[],
                        &[*chunk as u64],
                        None,
                        None,
                    )
                    .map_err(err)?,
                ),
                // C's rank-0 `H5Pset_chunk` fails, leaving it contiguous.
                AttrShape::Scalar => AttrSink::SwmrScalar(
                    swmr.create_dataset_typed(&path, ad.datatype(), &[])
                        .map_err(err)?,
                ),
            };
        }
        for ad in &self.attr_datasets {
            let layout_attrs = self.ndattr_dataset_layout_attrs(&ad.name);
            let path = ad.path();
            let (AttrSink::Swmr(index)
            | AttrSink::SwmrGrid { index, .. }
            | AttrSink::SwmrScalar(index)) = ad.sink
            else {
                unreachable!("every SWMR attribute dataset has a writer sink");
            };
            swmr.set_dataset_fill_value_bytes(index, &ad.fill())
                .map_err(err)?;
            let mut target = self.created_attrs.on(&path, SwmrDataset(swmr, index));
            for a in layout_attrs
                .iter()
                .filter(|a| a.source == crate::hdf5_layout::LayoutSource::Constant)
            {
                let _ = target.layout_constant(a);
            }
            write_ndattr_descriptors(&mut target, ad);
        }
        Ok(())
    }

    /// The `<attribute>` nodes of the layout dataset declared for the
    /// NDAttribute `name` (C `find_dset_ndattr`, NDFileHDF5.cpp:2792).
    fn ndattr_dataset_layout_attrs(&self, name: &str) -> &[crate::hdf5_layout::LayoutAttribute] {
        self.layout
            .as_ref()
            .and_then(|l| l.ndattribute_dataset(name))
            .map_or(&[], |(_, d)| d.attributes.as_slice())
    }

    /// Flush accumulated NDAttribute datasets into the open standard file,
    /// each in its group with its [`AttrShape`].
    fn flush_attribute_datasets(&mut self) -> ADResult<()> {
        if self.attr_datasets.is_empty() {
            return Ok(());
        }
        let layout_attrs: Vec<_> = self
            .attr_datasets
            .iter()
            .map(|ad| self.ndattr_dataset_layout_attrs(&ad.name))
            .collect();
        let h5file = match self.handle {
            Some(Hdf5Handle::Standard { ref file, .. }) => file,
            _ => return Ok(()),
        };
        // Group handles, cached by path; `build_layout_groups` created them.
        let mut group_cache: std::collections::HashMap<String, rust_hdf5::H5Group> =
            std::collections::HashMap::new();

        for (ad, layout_attrs) in self.attr_datasets.iter().zip(layout_attrs) {
            let group_path = &ad.group;
            let AttrSink::Buffer(buffer) = &ad.sink else {
                continue;
            };
            if !group_cache.contains_key(group_path) {
                let g = Self::open_write_group(h5file, group_path)?;
                group_cache.insert(group_path.clone(), g);
            }
            let group = &group_cache[group_path];

            let ds = match ad.data_type {
                NDAttrDataType::Int8 => write_standard_attribute_dataset::<i8>(group, ad, buffer),
                NDAttrDataType::UInt8 => write_standard_attribute_dataset::<u8>(group, ad, buffer),
                NDAttrDataType::Int16 => write_standard_attribute_dataset::<i16>(group, ad, buffer),
                NDAttrDataType::UInt16 => {
                    write_standard_attribute_dataset::<u16>(group, ad, buffer)
                }
                NDAttrDataType::Int32 => write_standard_attribute_dataset::<i32>(group, ad, buffer),
                NDAttrDataType::UInt32 => {
                    write_standard_attribute_dataset::<u32>(group, ad, buffer)
                }
                NDAttrDataType::Int64 => write_standard_attribute_dataset::<i64>(group, ad, buffer),
                NDAttrDataType::UInt64 => {
                    write_standard_attribute_dataset::<u64>(group, ad, buffer)
                }
                NDAttrDataType::Float32 => {
                    write_standard_attribute_dataset::<f32>(group, ad, buffer)
                }
                NDAttrDataType::Float64 => {
                    write_standard_attribute_dataset::<f64>(group, ad, buffer)
                }
                // C stores a string attribute as a fixed 256-byte `H5T_C_S1`
                // element, not a byte axis (NDFileHDF5AttributeDataset.cpp:321-323);
                // the buffer already holds one 256-byte field per element.
                NDAttrDataType::String => {
                    write_standard_attribute_dataset::<FixedStr256>(group, ad, buffer)
                }
            }?;

            // C writes the layout dataset's constant <attribute> nodes
            // (NDFileHDF5.cpp:2816), then up to four self-describing string
            // attributes, each only when non-empty (:2817-2822).
            let mut target = self.created_attrs.on(&ad.path(), &ds);
            for a in layout_attrs
                .iter()
                .filter(|a| a.source == crate::hdf5_layout::LayoutSource::Constant)
            {
                let _ = target.layout_constant(a);
            }
            write_ndattr_descriptors(&mut target, ad);
        }
        Ok(())
    }

    /// C `createPerformanceDataset` (NDFileHDF5.cpp:2596-2671) in the open
    /// standard file: `[1, 5]` doubles, unwritten. C creates it at open after
    /// the attribute datasets; this port creates those at close, so both
    /// close paths call this after them, and an aborted file keeps it
    /// unwritten as C's does (:1522-1557). `None` where C only warns and
    /// keeps none (:313-316, :2661-2668), as when an NDAttribute holds
    /// `timestamp`.
    fn create_performance_dataset(&self) -> Option<rust_hdf5::H5Dataset> {
        let perf_group = self
            .resolved_perf_group
            .as_deref()
            .filter(|_| self.cfg.store_performance)?;
        let Some(Hdf5Handle::Standard { ref file, .. }) = self.handle else {
            return None;
        };
        Self::open_write_group(file, perf_group)
            .ok()?
            .new_dataset::<f64>()
            .shape([1, 5])
            .chunk(&[self.attribute_chunking(), 5])
            // C `maxdims = {H5S_UNLIMITED, H5S_UNLIMITED}` (NDFileHDF5.cpp:2600).
            .max_shape(&[None, None])
            .create("timestamp")
            .ok()
    }

    /// Write the `timestamp` performance dataset (`[nframes, 5]` doubles)
    /// into the open standard file. Mirrors C++ `writePerformanceDataset`.
    fn flush_performance_dataset(&mut self) -> ADResult<()> {
        let Some(ds) = self.create_performance_dataset() else {
            return Ok(());
        };
        let n = self.perf_rows.len();
        // The written chunks extend the extent to whole chunks, so trim it to
        // the `n` rows C's `H5Dset_extent` sets (NDFileHDF5.cpp:2690).
        let hdf5_err = |e: rust_hdf5::Hdf5Error| {
            ADError::UnsupportedConversion(format!("HDF5 chunk write error: {}", e))
        };
        for (c, chunk) in self.performance_chunks().iter().enumerate() {
            ds.write_chunk_at(&[c, 0], chunk).map_err(hdf5_err)?;
        }
        ds.set_extent(&[n, 5]).map_err(hdf5_err)?;
        Ok(())
    }

    /// The `timestamp` rows as the `[chunking, 5]` little-endian chunks of
    /// C's two-unlimited-axis dataset (NDFileHDF5.cpp:2600, :2645-2647), a
    /// v2 B-tree written by grid coordinate; the last is zero-padded.
    fn performance_chunks(&self) -> Vec<Vec<u8>> {
        let band = self.attribute_chunking() * 5 * 8;
        let raw: Vec<u8> = self
            .perf_rows
            .iter()
            .flatten()
            .flat_map(|v| v.to_le_bytes())
            .collect();
        raw.chunks(band)
            .map(|rows| {
                let mut chunk = rows.to_vec();
                chunk.resize(band, 0);
                chunk
            })
            .collect()
    }

    /// C `verifyChunking` (NDFileHDF5Dataset.cpp:191-230) and the chunk C's
    /// `H5Dwrite_chunk` stores (:291-329): a compressed frame is
    /// direct-chunk-written only when its codec equals the dataset's, one
    /// chunk is exactly one frame and no extraDimChunk param is over 1, even
    /// one its axis clamps to 1 (:224-229); any other compressed frame is
    /// refused (:343-348). `None` for an uncompressed frame, which goes through the
    /// dataset's filter as C's H5Dwrite does.
    fn direct_chunk(
        &self,
        array: &NDArray,
        frame_dims: &[usize],
    ) -> Result<Option<Vec<u8>>, FrameError> {
        let Some(codec) = array.codec.as_ref() else {
            return Ok(None);
        };
        let chunk = &self.open_chunk;
        let lead_n = chunk.len() - frame_dims.len();
        let one_frame = chunk[..lead_n].iter().all(|&c| c == 1)
            && chunk[lead_n..]
                .iter()
                .zip(frame_dims)
                .all(|(&c, &d)| c == d.max(1))
            && !(self.multi_frame
                && self.cfg.extra_dims[..=self.cfg.n_extra_dims]
                    .iter()
                    .any(|d| d.chunk > 1));
        if !(one_frame && codecs_match(self.dataset_codec.as_ref(), Some(codec))) {
            return Err(FrameError::Dataset(ADError::Reported {
                lines: vec![
                    "NDFileHDF5Dataset::writeFile ERROR Unable to write pre-compressed \
                     data - mismatched chunk definition\n"
                        .to_owned(),
                ],
                message: "Unable to write pre-compressed data - mismatched chunk definition"
                    .to_owned(),
            }));
        }
        let total_bytes =
            frame_dims.iter().product::<usize>() * codec.original_data_type.element_size();
        Ok(Some(codec_chunk_bytes(
            codec,
            total_bytes,
            array.data().as_u8_slice(),
        )))
    }

    /// Write a frame in SWMR mode.
    fn write_swmr(&mut self, array: &NDArray) -> Result<(), FrameError> {
        let frame_dims: Vec<usize> = array.dims.iter().rev().map(|d| d.size).collect();
        let offsets = self.frame_positions(array).map_err(FrameError::Dataset)?;
        let positions = offsets.clone().filter(|_| self.multi_frame);
        if let Some(offsets) = &positions {
            let leaf = self.resolved_dataset_path.rsplit('/').next();
            self.check_positions(offsets, leaf.unwrap_or_default())
                .map_err(FrameError::Dataset)?;
        }
        let chunk_bytes = self.direct_chunk(array, &frame_dims)?;

        // This frame's 0-based odometer index (`frame_count` increments only
        // after `write_swmr` returns); read before the mutable handle borrow.
        let frame_idx = self.frame_count;
        let stored_elem_size = self.stored_elem_size;

        let (writer, ds_index, grid) = match self.handle {
            Some(Hdf5Handle::Swmr {
                ref mut writer,
                ds_index,
                ref mut grid,
                ..
            }) => (writer, ds_index, grid),
            _ => {
                return Err(FrameError::Before(ADError::UnsupportedConversion(
                    "no SWMR writer open".into(),
                )));
            }
        };

        // The grid frame's leading coordinates, and the extent they grow.
        let leading_coords = grid.as_mut().map(|g| {
            let coords = positions.unwrap_or_else(|| Self::unravel(frame_idx, &g.leading));
            for (e, c) in g.extent.iter_mut().zip(&coords) {
                *e = (*e).max(c + 1);
            }
            coords
        });
        if let Some(g) = grid.as_ref() {
            let dims: Vec<u64> = g
                .extent
                .iter()
                .chain(&g.frame_dims)
                .map(|&d| d as u64)
                .collect();
            writer.set_extent(ds_index, &dims).map_err(|e| {
                FrameError::Dataset(ADError::UnsupportedConversion(format!(
                    "SWMR set_extent error: {}",
                    e
                )))
            })?;
        }

        // A pre-compressed frame is one chunk, stored verbatim; filter_mask 0
        // because the codec applied the whole pipeline.
        if let Some(cb) = chunk_bytes {
            let stored = match (grid, leading_coords) {
                (Some(g), Some(leading_coords)) => {
                    let mut coords: Vec<u64> =
                        leading_coords.into_iter().map(|c| c as u64).collect();
                    coords.resize(g.chunk.len(), 0);
                    writer.write_chunk_raw_at(ds_index, &coords, &cb, 0)
                }
                _ => writer.append_frame_raw(ds_index, &cb, 0),
            };
            stored.map_err(|e| {
                FrameError::Dataset(ADError::UnsupportedConversion(format!(
                    "SWMR direct chunk write error: {}",
                    e
                )))
            })?;
        } else {
            // The SWMR streaming dataset declares a little-endian element type and
            // both `append_frame` and `write_chunk_at` copy the supplied `&[u8]`
            // verbatim; serialize to LE explicitly (see `nd_buffer_to_le_bytes`) so
            // the file is portable.
            let frame_bytes = widen_le(
                nd_buffer_to_le_bytes(array.data()),
                array.data().data_type(),
                stored_elem_size,
            );
            match (grid, leading_coords) {
                // A chunk shared with other frames is patched, as on the
                // standard path.
                (Some(g), Some(leading_coords))
                    if g.chunk[..g.leading.len()].iter().any(|&c| c > 1) =>
                {
                    let starts: Vec<u64> = leading_coords
                        .iter()
                        .map(|&c| c as u64)
                        .chain(g.frame_dims.iter().map(|_| 0))
                        .collect();
                    let counts: Vec<u64> = leading_coords
                        .iter()
                        .map(|_| 1)
                        .chain(g.frame_dims.iter().map(|&d| d as u64))
                        .collect();
                    writer
                        .write_slice(ds_index, &starts, &counts, &frame_bytes)
                        .map_err(|e| {
                            FrameError::Dataset(ADError::UnsupportedConversion(format!(
                                "SWMR write_slice error: {}",
                                e
                            )))
                        })?;
                }
                // Otherwise each frame is its own chunk(s), placed via
                // `write_chunk_at` as the standard path's `flush_band` does.
                (Some(g), Some(leading_coords)) => {
                    for (coords, tile) in Self::band_chunk_writes(
                        &leading_coords,
                        std::slice::from_ref(&frame_bytes),
                        &g.frame_dims,
                        &g.chunk,
                        g.elem_size,
                    ) {
                        let coords_u64: Vec<u64> = coords.iter().map(|&c| c as u64).collect();
                        writer
                            .write_chunk_at(ds_index, &coords_u64, &tile)
                            .map_err(|e| {
                                FrameError::Dataset(ADError::UnsupportedConversion(format!(
                                    "SWMR write_chunk_at error: {}",
                                    e
                                )))
                            })?;
                    }
                }
                _ => {
                    writer.append_frame(ds_index, &frame_bytes).map_err(|e| {
                        FrameError::Dataset(ADError::UnsupportedConversion(format!(
                            "SWMR append error: {}",
                            e
                        )))
                    })?;
                }
            }
        }

        // C `writeAttributeDataset(OnFrame)` follows the frame
        // (NDFileHDF5.cpp:1560-1570).
        if self.cfg.store_attributes {
            write_attribute_datasets(
                &mut self.attr_datasets,
                &self.file_attributes,
                LayoutWhen::OnFrame,
                offsets.as_deref(),
                Some(writer.as_mut()),
            )
            .map_err(FrameError::Dataset)?;
        }

        // Periodic flush
        let count = self.frame_count + 1; // will be incremented after return
        if self.cfg.flush_nth_frame > 0 && count % self.cfg.flush_nth_frame == 0 {
            flush_swmr_writer(&mut self.attr_datasets, writer).map_err(FrameError::Before)?;
        }
        Ok(())
    }

    /// C `writeFile`'s abort (NDFileHDF5.cpp:1519-1557): every later write to
    /// the dataset would fail as well, so the file is closed and the failure
    /// becomes [`ADError::Aborted`]. C has written each earlier frame and its
    /// OnFrame attributes already, so the bands and attribute values this port
    /// holds are written before the close; what C writes only in `closeFile`
    /// (performance rows, OnFileClose values) is not.
    fn abort(&mut self, failure: ADError) -> ADError {
        let mut lines = failure.lines().to_vec();
        lines.push("NDFileHDF5::writeFile ERROR: could not write to dataset. Aborting\n".into());
        if self.close_aborted().is_err() {
            lines.push("NDFileHDF5::writeFile ERROR: File did not close cleanly.\n".into());
        }
        self.handle = None;
        self.current_path = None;
        ADError::Aborted(Box::new(ADError::Reported {
            lines,
            message: failure.to_string(),
        }))
    }

    fn close_aborted(&mut self) -> Result<(), String> {
        let e = |e: &dyn std::fmt::Display| e.to_string();
        match self.handle {
            Some(Hdf5Handle::Standard { .. }) => {
                self.finalize_standard_datasets().map_err(|x| e(&x))?;
                self.flush_attribute_datasets().map_err(|x| e(&x))?;
                drop(self.create_performance_dataset());
                if let Some(Hdf5Handle::Standard { ref file, .. }) = self.handle {
                    self.build_layout_hardlinks(file).map_err(|x| e(&x))?;
                }
                if let Some(Hdf5Handle::Standard { file, detectors }) = self.handle.take() {
                    let closed = if self.fsync_on_close {
                        file.close()
                    } else {
                        file.close_no_sync()
                    };
                    drop(detectors);
                    closed.map_err(|x| e(&x))?;
                }
            }
            Some(Hdf5Handle::Swmr { .. }) => {
                if let Some(Hdf5Handle::Swmr { mut writer, .. }) = self.handle.take() {
                    flush_swmr_writer(&mut self.attr_datasets, &mut writer).map_err(|x| e(&x))?;
                    writer.close().map_err(|x| e(&x))?;
                }
            }
            None => {}
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

/// A compression param C `configureCompression` sets from a frame's codec.
#[derive(Clone, Copy, Debug, PartialEq)]
enum CompressionParam {
    Type,
    ZLevel,
    BloscLevel,
    BloscShuffle,
    BloscCompressor,
    NbitOffset,
    NbitPrecision,
}

/// How a frame write failed: `Dataset` is C's dataset step
/// (`extendDataSet` or the dataset's `writeFile`), which aborts the file;
/// `Before` is anything that returns ahead of it.
enum FrameError {
    Before(ADError),
    Dataset(ADError),
}

impl From<ADError> for FrameError {
    fn from(e: ADError) -> Self {
        Self::Before(e)
    }
}

/// Whether two NDArray codecs are equal for the purpose of C
/// `NDFileHDF5Dataset::verifyChunking` (NDFileHDF5Dataset.cpp:194-200), which
/// compares `pArray->codec != this->codec`. C `Codec_t::operator!=` (Codec.h)
/// equates name, level, shuffle and compressor; `original_data_type` is not part
/// of the codec identity (it travels with the array's element type). Two absent
/// codecs (an uncompressed file) also match.
fn codecs_match(a: Option<&Codec>, b: Option<&Codec>) -> bool {
    match (a, b) {
        (None, None) => true,
        (Some(x), Some(y)) => {
            x.name == y.name
                && x.level == y.level
                && x.shuffle == y.shuffle
                && x.compressor == y.compressor
        }
        _ => false,
    }
}

/// Build the on-disk HDF5 chunk byte stream for one pre-compressed frame,
/// matching C `NDFileHDF5Dataset::writeFile` (NDFileHDF5Dataset.cpp:291-329).
/// `payload` is the codec's compressed output (this port stores it in the
/// array's collapsed `U8` buffer); `uncompressed_total_bytes` is the size of one
/// uncompressed frame (C `NDArrayInfo::totalBytes`).
///
/// - **LZ4**: this port's `compress_lz4` emits a raw LZ4 block with no header
///   (C `pArray->pData` likewise), so prepend the 16-byte big-endian header the
///   HDF5 LZ4 filter expects — uncompressed size (u64), block size = uncompressed
///   size (u32; frames assumed < 1 GiB, as C does), compressed size (u32) — then
///   the block.
/// - **BSLZ4**: this port's `compress_bslz4` emits the headerless canonical
///   bitshuffle+LZ4 stream (C `pArray->pData` likewise — the per-block
///   `[u32 nbytes_BE]` headers are part of the stream, but the chunk-level header
///   is not), so prepend the 12-byte big-endian header the HDF5 bitshuffle filter
///   expects — uncompressed size (u64) and the bitshuffle block size **in bytes**
///   (u32 = `block_elems * elem_size`; C hardcodes its 8192 default,
///   NDFileHDF5Dataset.cpp:316-328) — then the stream.
/// - **BLOSC / JPEG**: this port's codecs already emit the exact on-disk filter
///   format (blosc and jpeg streams are self-describing), so the payload is
///   written verbatim — the same bytes C produces after its per-codec header step.
fn codec_chunk_bytes(codec: &Codec, uncompressed_total_bytes: usize, payload: &[u8]) -> Vec<u8> {
    match codec.name {
        CodecName::LZ4 => {
            let mut out = Vec::with_capacity(16 + payload.len());
            out.extend_from_slice(&(uncompressed_total_bytes as u64).to_be_bytes());
            out.extend_from_slice(&(uncompressed_total_bytes as u32).to_be_bytes());
            out.extend_from_slice(&(payload.len() as u32).to_be_bytes());
            out.extend_from_slice(payload);
            out
        }
        CodecName::BSLZ4 => {
            let elem_size = codec.original_data_type.element_size();
            let block_bytes = crate::codec::bshuf_default_block_size(elem_size) * elem_size;
            let mut out = Vec::with_capacity(12 + payload.len());
            out.extend_from_slice(&(uncompressed_total_bytes as u64).to_be_bytes());
            out.extend_from_slice(&(block_bytes as u32).to_be_bytes());
            out.extend_from_slice(payload);
            out
        }
        _ => payload.to_vec(),
    }
}

/// Serialize an NDArray data buffer to **little-endian** bytes.
///
/// `rust-hdf5` 0.2.15 records every numeric datatype message as little-endian
/// (`Endianness::LittleEndian`) and its only chunked-write API, `write_chunk`,
/// copies the supplied `&[u8]` verbatim into the chunk with no byte-swap.
/// `NDDataBuffer::as_u8_slice()` returns the buffer in *host* byte order, so
/// feeding it directly into a typed chunked dataset is correct only on a
/// little-endian host. This helper makes the on-disk bytes match the declared
/// LE datatype on every host: on LE it is a verbatim copy, on BE it swaps each
/// element. Used for every typed-dataset chunk write where no typed
/// chunked-write path exists in the crate.
fn nd_buffer_to_le_bytes(buf: &NDDataBuffer) -> Vec<u8> {
    match buf.view() {
        NDDataView::I8(v) => v.iter().map(|&x| x as u8).collect(),
        NDDataView::U8(v) => v.to_vec(),
        NDDataView::I16(v) => v.iter().flat_map(|&x| x.to_le_bytes()).collect(),
        NDDataView::U16(v) => v.iter().flat_map(|&x| x.to_le_bytes()).collect(),
        NDDataView::I32(v) => v.iter().flat_map(|&x| x.to_le_bytes()).collect(),
        NDDataView::U32(v) => v.iter().flat_map(|&x| x.to_le_bytes()).collect(),
        NDDataView::I64(v) => v.iter().flat_map(|&x| x.to_le_bytes()).collect(),
        NDDataView::U64(v) => v.iter().flat_map(|&x| x.to_le_bytes()).collect(),
        NDDataView::F32(v) => v.iter().flat_map(|&x| x.to_le_bytes()).collect(),
        NDDataView::F64(v) => v.iter().flat_map(|&x| x.to_le_bytes()).collect(),
    }
}

/// The HDF5 datatype of an `NDDataType` element.
fn native_datatype(data_type: NDDataType) -> DatatypeMessage {
    use rust_hdf5::types::H5Type;
    match data_type {
        NDDataType::Int8 => i8::hdf5_type(),
        NDDataType::UInt8 => u8::hdf5_type(),
        NDDataType::Int16 => i16::hdf5_type(),
        NDDataType::UInt16 => u16::hdf5_type(),
        NDDataType::Int32 => i32::hdf5_type(),
        NDDataType::UInt32 => u32::hdf5_type(),
        NDDataType::Int64 => i64::hdf5_type(),
        NDDataType::UInt64 => u64::hdf5_type(),
        NDDataType::Float32 => f32::hdf5_type(),
        NDDataType::Float64 => f64::hdf5_type(),
    }
}

/// `le` (elements of `data_type`) with each element extended to `stored`
/// bytes: sign-extended for a signed integer, zero-filled otherwise.
///
/// C defect, not copied: for an nbit datatype HDF5 widened, C hands that
/// datatype to H5Dwrite and H5Pset_fill_value as the memory type
/// (NDFileHDF5.cpp:1520, :3895) and reads past each native element. Here the
/// value is widened, and the nbit filter keeps bits [offset, offset+precision)
/// of it as it does of the raw element when nothing widens.
fn widen_le(le: Vec<u8>, data_type: NDDataType, stored: usize) -> Vec<u8> {
    let native = data_type.element_size();
    if stored == native {
        return le;
    }
    let signed = matches!(
        data_type,
        NDDataType::Int8 | NDDataType::Int16 | NDDataType::Int32 | NDDataType::Int64
    );
    let mut out = Vec::with_capacity(le.len() / native * stored);
    for e in le.chunks_exact(native) {
        out.extend_from_slice(e);
        let pad = if signed && e[native - 1] & 0x80 != 0 {
            0xFF
        } else {
            0
        };
        out.resize(out.len() + stored - native, pad);
    }
    out
}

/// C's `HDF5_fillValue` cast to the element type (`typeNd2Hdf`,
/// NDFileHDF5.cpp:3491-3538), widened to `stored` bytes, in host order as
/// `fill_value_bytes` takes it.
fn stored_fill(fill: f64, data_type: NDDataType, stored: usize) -> Vec<u8> {
    let le = ad_core_rs::convert::c_cast_f64_le(fill, data_type);
    let mut bytes = widen_le(le, data_type, stored);
    if cfg!(target_endian = "big") {
        bytes.reverse();
    }
    bytes
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

/// One standard-path attribute dataset of `T`, holding the elements of
/// `buffer` in `ad.shape`.
fn write_standard_attribute_dataset<T: rust_hdf5::types::H5Type>(
    group: &rust_hdf5::H5Group,
    ad: &AttributeDataset,
    buffer: &[u8],
) -> ADResult<rust_hdf5::H5Dataset> {
    let err = |e: rust_hdf5::Hdf5Error| {
        ADError::UnsupportedConversion(format!("HDF5 attribute dataset error: {e}"))
    };
    let es = ad.element_size();
    let n = buffer.len() / es;
    let builder = group.new_dataset::<T>();
    match &ad.shape {
        AttrShape::Series { chunk } => {
            // The chunk is not clamped to the element count: the axis is
            // unlimited (`calculateAttributeChunking` → numCapture).
            let ds = builder
                .shape([n])
                .chunk(&[*chunk])
                .max_shape(&[None])
                .create(&ad.dset_name)
                .map_err(err)?;
            write_chunked_buffer(&ds, buffer, chunk * es)?;
            Ok(ds)
        }
        AttrShape::Grid { dims, chunk } => {
            let extent = &ad.extent;
            let ds = builder
                .shape(extent)
                .chunk(chunk)
                .max_shape(&vec![None; dims.len()])
                .create(&ad.dset_name)
                .map_err(err)?;
            let fill = ad.fill();
            let cells = grid_cells(buffer, dims, &fill);
            let tiles: Vec<usize> = extent
                .iter()
                .zip(chunk)
                .map(|(e, c)| e.div_ceil(*c))
                .collect();
            for t in 0..tiles.iter().product() {
                let coords = Hdf5Writer::unravel(t, &tiles);
                ds.write_chunk_at(&coords, &grid_tile(&cells, dims, chunk, &coords, &fill))
                    .map_err(err)?;
            }
            // write_chunk_at rounds the extent up to whole chunks.
            ds.set_extent(extent).map_err(err)?;
            Ok(ds)
        }
        AttrShape::Scalar => {
            let ds = builder.scalar().create(&ad.dset_name).map_err(err)?;
            // Every write lands on the one element; the last stands.
            if let Some(last) = buffer.len().checked_sub(es) {
                ds.write_raw_bytes(&buffer[last..]).map_err(err)?;
            }
            Ok(ds)
        }
    }
}

/// Attach the four self-describing string HDF5 attributes C writes on each
/// NDAttribute dataset — `NDAttrName`, `NDAttrDescription`, `NDAttrSourceType`,
/// `NDAttrSource` (NDFileHDF5.cpp:2715, 2817-2822) — each only when non-empty
/// (C `if (strlen <= 0) continue`).
fn write_ndattr_descriptors(target: &mut impl AttrTarget, ad: &AttributeDataset) {
    for (aname, avalue) in [
        ("NDAttrName", ad.name.as_str()),
        ("NDAttrDescription", ad.description.as_str()),
        ("NDAttrSourceType", ad.source_type.as_str()),
        ("NDAttrSource", ad.source.as_str()),
    ] {
        if avalue.is_empty() {
            continue;
        }
        let _ = target.string(aname, avalue);
    }
}

/// storeOnOpenCloseAttribute (NDFileHDF5.cpp:587-618): a live NDAttribute
/// value as an attribute of its own type (C `typeNd2Hdf`); an undefined one
/// is skipped.
fn write_ndattr_value(
    target: &mut impl AttrTarget,
    name: &str,
    value: &NDAttrValue,
) -> rust_hdf5::Result<()> {
    match value {
        NDAttrValue::Int8(v) => target.scalar(name, v),
        NDAttrValue::UInt8(v) => target.scalar(name, v),
        NDAttrValue::Int16(v) => target.scalar(name, v),
        NDAttrValue::UInt16(v) => target.scalar(name, v),
        NDAttrValue::Int32(v) => target.scalar(name, v),
        NDAttrValue::UInt32(v) => target.scalar(name, v),
        NDAttrValue::Int64(v) => target.scalar(name, v),
        NDAttrValue::UInt64(v) => target.scalar(name, v),
        NDAttrValue::Float32(v) => target.scalar(name, v),
        NDAttrValue::Float64(v) => target.scalar(name, v),
        NDAttrValue::String(s) => target.string(name, s),
        NDAttrValue::Undefined => Ok(()),
    }
}

/// An object NDFileHDF5 hangs attributes on: a group or a dataset, in a
/// standard or a SWMR file.
trait AttrTarget {
    fn scalar<T: H5Type>(&mut self, name: &str, value: &T) -> rust_hdf5::Result<()>;
    fn array<T: H5Type>(&mut self, name: &str, values: &[T]) -> rust_hdf5::Result<()>;
    fn string(&mut self, name: &str, value: &str) -> rust_hdf5::Result<()>;

    /// writeH5attrInt32 / writeH5attrFloat64 (NDFileHDF5.cpp:1117-1285): one
    /// value is a scalar, any other count a 1-D array.
    fn numbers<T: H5Type>(&mut self, name: &str, values: &[T]) -> rust_hdf5::Result<()> {
        match values {
            [one] => self.scalar(name, one),
            _ => self.array(name, values),
        }
    }

    /// A layout `constant` attribute as writeHdfAttributes
    /// (NDFileHDF5.cpp:764-797) types it.
    fn layout_constant(
        &mut self,
        a: &crate::hdf5_layout::LayoutAttribute,
    ) -> rust_hdf5::Result<()> {
        match LayoutConst::new(a.data_type, &a.value) {
            LayoutConst::Int(v) => self.numbers(&a.name, &v),
            LayoutConst::Float(v) => self.numbers(&a.name, &v),
            LayoutConst::Str(s) => self.string(&a.name, s),
        }
    }
}

/// The attribute names created on each object of the open file, keyed by the
/// object's full name. `H5Acreate2` refuses a name its object already has, so
/// C keeps the first value written under it.
#[derive(Default)]
struct CreatedAttrs(Mutex<std::collections::HashSet<(String, String)>>);

impl CreatedAttrs {
    /// `target`, the object named `object`, creating attributes as
    /// `H5Acreate2` does.
    fn on<T: AttrTarget>(&self, object: &str, target: T) -> Gated<'_, T> {
        Gated {
            created: self,
            object: object.trim_start_matches('/').to_owned(),
            target,
        }
    }
}

/// An [`AttrTarget`] whose attribute creates go through [`CreatedAttrs`].
struct Gated<'a, T> {
    created: &'a CreatedAttrs,
    object: String,
    target: T,
}

impl<T: AttrTarget> Gated<'_, T> {
    fn create(
        &mut self,
        name: &str,
        write: impl FnOnce(&mut T) -> rust_hdf5::Result<()>,
    ) -> rust_hdf5::Result<()> {
        let key = (self.object.clone(), name.to_owned());
        if self.created.0.lock().contains(&key) {
            return Err(rust_hdf5::Hdf5Error::InvalidState(format!(
                "unable to create attribute: {name}"
            )));
        }
        write(&mut self.target)?;
        self.created.0.lock().insert(key);
        Ok(())
    }
}

impl<T: AttrTarget> AttrTarget for Gated<'_, T> {
    fn scalar<V: H5Type>(&mut self, name: &str, value: &V) -> rust_hdf5::Result<()> {
        self.create(name, |t| t.scalar(name, value))
    }
    fn array<V: H5Type>(&mut self, name: &str, values: &[V]) -> rust_hdf5::Result<()> {
        self.create(name, |t| t.array(name, values))
    }
    fn string(&mut self, name: &str, value: &str) -> rust_hdf5::Result<()> {
        self.create(name, |t| t.string(name, value))
    }
}

/// A layout `constant` value as C types it: an int32 or double list read by
/// stream extraction (NDFileHDF5.cpp:857-1035, :1117-1285), or the text.
enum LayoutConst<'a> {
    Int(Vec<i32>),
    Float(Vec<f64>),
    Str(&'a str),
}

impl<'a> LayoutConst<'a> {
    fn new(data_type: crate::hdf5_layout::LayoutDataType, value: &'a str) -> Self {
        use crate::hdf5_layout::LayoutDataType;
        match data_type {
            LayoutDataType::Int => Self::Int(stream_numbers(value, read_int)),
            LayoutDataType::Float => Self::Float(stream_numbers(value, read_double)),
            LayoutDataType::String => Self::Str(value),
        }
    }
}

/// writeH5dsetInt32 / writeH5dsetFloat64 store one value in a scalar
/// dataspace and any other count in a 1-D one.
fn const_list_dims(n: usize) -> Vec<usize> {
    if n == 1 { Vec::new() } else { vec![n] }
}

/// writeDefaultDatasetAttributes (NDFileHDF5.cpp:3697-3753): the NDArray
/// geometry a detector dataset carries, native dimension order, one int32 per
/// dimension. C adds each to the dataset's layout element, which keeps an
/// attribute of that name the layout already gave it (Element::add_attribute).
fn ndarray_default_attrs(
    array: &NDArray,
    layout_attrs: &[crate::hdf5_layout::LayoutAttribute],
) -> Vec<(&'static str, Vec<i32>)> {
    let dims = &array.dims;
    [
        ("NDArrayNumDims", vec![dims.len() as i32]),
        (
            "NDArrayDimOffset",
            dims.iter().map(|d| d.offset as i32).collect(),
        ),
        (
            "NDArrayDimBinning",
            dims.iter().map(|d| d.binning as i32).collect(),
        ),
        (
            "NDArrayDimReverse",
            dims.iter().map(|d| d.reverse as i32).collect(),
        ),
    ]
    .into_iter()
    .filter(|(name, _)| !layout_attrs.iter().any(|a| a.name == *name))
    .collect()
}

/// `H5Tcopy(H5T_C_S1)` with `H5Tset_size(size)` and
/// `H5Tset_strpad(H5T_STR_NULLTERM)`.
fn c_s1_nullterm(size: usize) -> DatatypeMessage {
    DatatypeMessage::FixedString {
        size: size as u32,
        padding: 0,
        charset: 0,
    }
}

/// A C string as NDFileHDF5 stores it (writeStringAttribute, writeH5attrStr,
/// writeH5dsetStr): the text and its terminator under [`c_s1_nullterm`].
fn c_string(value: &str) -> (DatatypeMessage, Vec<u8>) {
    let bytes = [value.as_bytes(), b"\0"].concat();
    (c_s1_nullterm(bytes.len()), bytes)
}

/// writeHdfConstDataset (NDFileHDF5.cpp:799-822): a `<dataset
/// source="constant">` in a standard file, contiguous as `H5Dcreate2` with
/// the default property lists makes it, then its constant `<attribute>`
/// nodes as createTree writes them (:686-687).
fn write_const_dataset(
    group: &rust_hdf5::H5Group,
    d: &crate::hdf5_layout::LayoutDataset,
    created: &CreatedAttrs,
) -> rust_hdf5::Result<rust_hdf5::H5Dataset> {
    fn list<T: H5Type>(
        group: &rust_hdf5::H5Group,
        name: &str,
        values: &[T],
    ) -> rust_hdf5::Result<rust_hdf5::H5Dataset> {
        let ds = group
            .new_dataset::<T>()
            .shape(const_list_dims(values.len()))
            .create(name)?;
        ds.write_raw(values)?;
        Ok(ds)
    }
    let ds = match LayoutConst::new(d.data_type, &d.value) {
        LayoutConst::Int(v) => list(group, &d.name, &v)?,
        LayoutConst::Float(v) => list(group, &d.name, &v)?,
        LayoutConst::Str(s) => {
            // writeH5dsetStr: in a 1-D space of one.
            let (datatype, bytes) = c_string(s);
            let ds = group
                .new_dataset::<u8>()
                .datatype(datatype)
                .shape([1])
                .create(&d.name)?;
            ds.write_raw_bytes(&bytes)?;
            ds
        }
    };
    let mut target = created.on(&format!("{}/{}", group.name(), d.name), &ds);
    for a in d.constant_attributes() {
        let _ = target.layout_constant(a);
    }
    Ok(ds)
}

/// [`write_const_dataset`] against a `SwmrFileWriter`, `path` being the
/// dataset's full name; returns its index.
fn write_swmr_const_dataset(
    swmr: &mut SwmrFileWriter,
    path: &str,
    d: &crate::hdf5_layout::LayoutDataset,
    created: &CreatedAttrs,
) -> rust_hdf5::Result<usize> {
    let dims = |n: usize| -> Vec<u64> { const_list_dims(n).iter().map(|&v| v as u64).collect() };
    let index = match LayoutConst::new(d.data_type, &d.value) {
        LayoutConst::Int(v) => swmr.write_dataset(path, &dims(v.len()), &v)?,
        LayoutConst::Float(v) => swmr.write_dataset(path, &dims(v.len()), &v)?,
        LayoutConst::Str(s) => {
            let (datatype, bytes) = c_string(s);
            swmr.write_dataset_typed(path, datatype, &[1], &bytes)?
        }
    };
    let mut target = created.on(path, SwmrDataset(swmr, index));
    for a in d.constant_attributes() {
        let _ = target.layout_constant(a);
    }
    Ok(index)
}

/// C `openFile`'s refusal of a layout `createFileLayout` failed, after the
/// lines that failure printed (NDFileHDF5.cpp:278-282).
fn layout_failed(mut lines: Vec<String>, why: &str) -> ADError {
    lines
        .push("NDFileHDF5::openFile ERROR Failed to create the specified file layout\n".to_owned());
    ADError::Reported {
        lines,
        message: format!("Failed to create the specified file layout: {why}"),
    }
}

/// C `writeHdfAttributes`'s line for each attribute without a known source,
/// whose `int8` type no branch writes (NDFileHDF5.cpp:789-792).
fn post_unknown_types<'a>(
    posts: &Mutex<OpenPosts>,
    attrs: impl IntoIterator<Item = &'a crate::hdf5_layout::LayoutAttribute>,
) {
    posts.lock().lines.extend(
        attrs
            .into_iter()
            .filter(|a| a.source == crate::hdf5_layout::LayoutSource::Unset)
            .map(|a| {
                format!(
                    "NDFileHDF5::writeHdfAttributes unknown type: unable to create attribute: {}\n",
                    a.name
                )
            }),
    );
}

impl AttrTarget for &rust_hdf5::H5Group {
    fn scalar<T: H5Type>(&mut self, name: &str, value: &T) -> rust_hdf5::Result<()> {
        self.set_attr_numeric(name, value)
    }
    fn array<T: H5Type>(&mut self, name: &str, values: &[T]) -> rust_hdf5::Result<()> {
        self.set_attr_array_numeric(name, values)
    }
    fn string(&mut self, name: &str, value: &str) -> rust_hdf5::Result<()> {
        let (datatype, bytes) = c_string(value);
        self.set_attr_typed(name, datatype, bytes)
    }
}

impl AttrTarget for &rust_hdf5::H5Dataset {
    fn scalar<T: H5Type>(&mut self, name: &str, value: &T) -> rust_hdf5::Result<()> {
        self.new_attr::<T>()
            .shape(())
            .create(name)?
            .write_numeric(value)
    }
    fn array<T: H5Type>(&mut self, name: &str, values: &[T]) -> rust_hdf5::Result<()> {
        self.new_attr::<T>()
            .shape([values.len()])
            .create(name)?
            .write_array(values)
    }
    fn string(&mut self, name: &str, value: &str) -> rust_hdf5::Result<()> {
        let (datatype, bytes) = c_string(value);
        self.set_attr_typed(name, datatype, bytes)
    }
}

/// A SWMR dataset, addressed by its index.
struct SwmrDataset<'a>(&'a mut SwmrFileWriter, usize);

impl AttrTarget for SwmrDataset<'_> {
    fn scalar<T: H5Type>(&mut self, name: &str, value: &T) -> rust_hdf5::Result<()> {
        self.0.set_dataset_attr_numeric(self.1, name, value)
    }
    fn array<T: H5Type>(&mut self, name: &str, values: &[T]) -> rust_hdf5::Result<()> {
        self.0
            .set_dataset_attr_array(self.1, name, &[values.len() as u64], values)
    }
    fn string(&mut self, name: &str, value: &str) -> rust_hdf5::Result<()> {
        let (datatype, bytes) = c_string(value);
        self.0.set_dataset_attr_typed(self.1, name, datatype, bytes)
    }
}

/// A SWMR group, addressed by its absolute path.
struct SwmrGroup<'a>(&'a mut SwmrFileWriter, &'a str);

impl AttrTarget for SwmrGroup<'_> {
    fn scalar<T: H5Type>(&mut self, name: &str, value: &T) -> rust_hdf5::Result<()> {
        self.0.set_group_attr_numeric(self.1, name, value)
    }
    fn array<T: H5Type>(&mut self, name: &str, values: &[T]) -> rust_hdf5::Result<()> {
        self.0
            .set_group_attr_array(self.1, name, &[values.len() as u64], values)
    }
    fn string(&mut self, name: &str, value: &str) -> rust_hdf5::Result<()> {
        let (datatype, bytes) = c_string(value);
        self.0.set_group_attr_typed(self.1, name, datatype, bytes)
    }
}

/// C's `while (ss >> v) { vect.push_back(v); if (ss.peek() == ',') ss.ignore(); }`
/// over a layout constant (NDFileHDF5.cpp:1135-1142): each read skips
/// whitespace and takes the longest number, the first failed read ends it.
fn stream_numbers<T>(value: &str, read: fn(&str) -> Option<(T, usize)>) -> Vec<T> {
    let mut out = Vec::new();
    let mut rest = value;
    loop {
        rest = rest.trim_start_matches([' ', '\t', '\n', '\x0b', '\x0c', '\r']);
        let Some((v, len)) = read(rest) else {
            return out;
        };
        out.push(v);
        rest = &rest[len..];
        rest = rest.strip_prefix(',').unwrap_or(rest);
    }
}

fn ascii_digits(s: &[u8]) -> usize {
    s.iter().take_while(|c| c.is_ascii_digit()).count()
}

/// `ss >> int`: sign and digits; an out-of-range value fails the read.
fn read_int(s: &str) -> Option<(i32, usize)> {
    let b = s.as_bytes();
    let sign = usize::from(matches!(b.first(), Some(b'+' | b'-')));
    let len = sign + ascii_digits(&b[sign..]);
    s[..len].parse().ok().map(|v| (v, len))
}

/// `ss >> double`: libstdc++ takes the exponent once a mantissa digit is
/// seen, then fails the read unless strtod consumes all of it and the value
/// is finite.
fn read_double(s: &str) -> Option<(f64, usize)> {
    let b = s.as_bytes();
    let mut len = usize::from(matches!(b.first(), Some(b'+' | b'-')));
    let mut mantissa = ascii_digits(&b[len..]);
    len += mantissa;
    if b.get(len) == Some(&b'.') {
        let frac = ascii_digits(&b[len + 1..]);
        mantissa += frac;
        len += 1 + frac;
    }
    if mantissa > 0 && matches!(b.get(len), Some(b'e' | b'E')) {
        len += 1;
        len += usize::from(matches!(b.get(len), Some(b'+' | b'-')));
        len += ascii_digits(&b[len..]);
    }
    s[..len]
        .parse::<f64>()
        .ok()
        .filter(|v| v.is_finite())
        .map(|v| (v, len))
}

/// Write a layout group's `constant`-sourced `<attribute>` nodes (e.g. the
/// NeXus `NX_class` markers) to an open standard-mode group.
/// `ndattribute`-sourced group attributes carry per-frame values and are
/// written separately by [`write_ndattr_value`]. Errors are non-fatal.
fn write_group_constant_attrs(
    target: &mut impl AttrTarget,
    attrs: &[crate::hdf5_layout::LayoutAttribute],
) {
    use crate::hdf5_layout::LayoutSource;
    for a in attrs {
        if a.source == LayoutSource::Constant {
            let _ = target.layout_constant(a);
        }
    }
}

/// SWMR counterpart of [`write_group_constant_attrs`], `group_path` naming
/// the group in its errors.
fn write_swmr_group_constant_attrs(
    target: &mut impl AttrTarget,
    group_path: &str,
    attrs: &[crate::hdf5_layout::LayoutAttribute],
) -> ADResult<()> {
    use crate::hdf5_layout::LayoutSource;
    for a in attrs {
        if a.source != LayoutSource::Constant {
            continue;
        }
        target.layout_constant(a).map_err(|e| {
            ADError::UnsupportedConversion(format!(
                "SWMR layout group attribute '{}/{}': {}",
                group_path, a.name, e
            ))
        })?;
    }
    Ok(())
}

impl Default for Hdf5Writer {
    fn default() -> Self {
        Self::new()
    }
}

/// C `addDefaultAttributes` (NDFileHDF5.cpp:3663-3677).
fn default_attributes(array: &NDArray) -> NDAttributeList {
    let mut list = NDAttributeList::new();
    for (name, description, value) in [
        (
            "NDArrayUniqueId",
            "The unique ID of the NDArray",
            NDAttrValue::Int32(array.unique_id),
        ),
        (
            "NDArrayTimeStamp",
            "The timestamp of the NDArray as float64",
            NDAttrValue::Float64(array.time_stamp),
        ),
        (
            "NDArrayEpicsTSSec",
            "The NDArray EPICS timestamp seconds past epoch",
            NDAttrValue::UInt32(array.timestamp.sec),
        ),
        (
            "NDArrayEpicsTSnSec",
            "The NDArray EPICS timestamp nanoseconds",
            NDAttrValue::UInt32(array.timestamp.nsec),
        ),
    ] {
        list.add(NDAttribute::new_static(
            name,
            description,
            NDAttrSource::Driver,
            value,
        ));
    }
    list
}

impl NDFileWriter for Hdf5Writer {
    fn open_file(&mut self, path: &Path, mode: NDFileOpenMode, array: &NDArray) -> ADResult<()> {
        // C resets the flush counter and verifies the layout before it
        // returns for any refusal (:185-217); a refused layout creates no file.
        self.read_settings();
        self.swmr_cb_counter = 0;
        let layout = verify_layout(&self.cfg.layout_spec);
        self.open_posts.lock().posts.extend([
            OpenPost::SwmrCbCounter(0),
            match layout {
                Ok(msg) => OpenPost::Layout(true, msg),
                Err(fault) => OpenPost::Layout(false, fault.message()),
            },
        ]);
        // A negative capture count is refused ahead of the layout's own
        // line (:205-211).
        let mut lines: Vec<String> = (self.num_capture < 0)
            .then(|| {
                format!(
                    "NDFileHDF5::openFile Invalid number of frames to capture: {}. \
                     Please specify a number >= 0\n",
                    self.num_capture
                )
            })
            .into_iter()
            .collect();
        let refused = match layout {
            Ok(_) if lines.is_empty() => Ok(()),
            Ok(_) => Err(ADError::Reported {
                lines,
                message: "Invalid number of frames to capture".into(),
            }),
            Err(fault) => {
                lines.push(fault.line().to_owned());
                Err(ADError::Reported {
                    lines,
                    message: fault.message().to_owned(),
                })
            }
        };
        // C drops the capture, NDFileCapture and NDWriteFile to 0, on a read
        // or an append (NDFileHDF5.cpp:192-203).
        if mode.contains(NDFileOpenMode::READ) || mode.contains(NDFileOpenMode::APPEND) {
            return Err(ADError::Aborted(Box::new(refused.err().unwrap_or_else(
                || ADError::UnsupportedConversion("NDFileHDF5 opens files only to write".into()),
            ))));
        }
        refused?;
        self.current_path = Some(path.to_path_buf());
        self.multi_frame = mode.contains(NDFileOpenMode::MULTIPLE);
        self.frame_count = 0;
        self.total_runtime = 0.0;
        self.total_bytes = 0;
        self.open_data_type = None;
        self.stored_elem_size = 0;
        self.open_frame_dims = None;
        self.dataset_codec = None;
        self.open_chunk.clear();
        self.perf_rows.clear();
        self.perf_prev = None;
        self.perf_first = None;
        self.attr_datasets.clear();
        self.created_attrs = CreatedAttrs::default();
        // C `openFile` clears `pFileAttributes`, adds the default attributes,
        // the plugin's set and this frame's list (NDFileHDF5.cpp:243-252);
        // every attribute read for the rest of the file goes through it, not
        // through the frame.
        let mut set = default_attributes(array);
        set.copy_from(&self.plugin_attributes);
        self.file_attributes.open(&set, array);
        if let Err(e) = self.load_layout() {
            // C creates the file before it loads the layout (NDFileHDF5.cpp:
            // 271, :279), so a failed layout leaves the empty file behind.
            drop(H5File::create(path).map_err(|e| Self::create_failed(path, e))?);
            return Err(e);
        }
        // Resolve where image/attribute/performance datasets land for this
        // file: the loaded layout XML tree, or the flat root default.
        self.resolve_layout_paths();
        // Seed the open-time NDAttribute values for any layout
        // `<attribute source="ndattribute">` element-attrs (ADP-79).
        self.seed_ndattr_element_values();

        if self.cfg.swmr_mode {
            self.open_swmr(path, array)
        } else {
            let h5file = H5File::create(path).map_err(|e| Self::create_failed(path, e))?;
            self.handle = Some(Hdf5Handle::Standard {
                file: h5file,
                detectors: std::collections::HashMap::new(),
            });
            // A failed open closes the file it built, which stays on disk as
            // C's does; C holds it open until the next openFile (:221).
            let mut opening = Finalize::new(self, |w: &mut Self| w.handle = None);
            // C `createFileLayout` from the open array, then
            // `createAttributeDataset` and `writeAttributeDataset(OnFileOpen)`
            // (NDFileHDF5.cpp:278, :297-299).
            opening.run(|w| {
                w.create_detector_datasets(array)?;
                w.create_attribute_datasets();
                write_attribute_datasets(
                    &mut w.attr_datasets,
                    &w.file_attributes,
                    LayoutWhen::OnFileOpen,
                    None,
                    None,
                )
            })?;
            opening.disarm();
            Ok(())
        }
    }

    fn set_num_capture(&mut self, n: i32) {
        self.num_capture = n;
    }

    fn set_plugin_attributes(&mut self, attributes: NDAttributeList) {
        self.plugin_attributes = attributes;
    }

    fn write_file(&mut self, array: &Arc<NDArray>) -> ADResult<()> {
        self.read_settings();
        let start = std::time::Instant::now();

        // C `writeFile` merges the plugin's set, the default attributes and
        // this frame into `pFileAttributes` before it writes anything
        // (NDFileHDF5.cpp:1420-1437).
        let mut set = self.plugin_attributes.clone();
        set.copy_from(&default_attributes(array));
        self.file_attributes.frame(&set, array);

        let is_swmr = matches!(self.handle, Some(Hdf5Handle::Swmr { .. }));
        let written = if is_swmr {
            self.write_swmr(array)
        } else {
            self.write_standard(array)
        };
        match written {
            Ok(()) => {}
            Err(FrameError::Before(e)) => return Err(e),
            Err(FrameError::Dataset(e)) => return Err(self.abort(e)),
        }
        // Track the latest NDAttribute values for `when="OnFileClose"`
        // element-attrs (ADP-79); no-op unless the layout declares any.
        self.update_ndattr_element_values();
        self.frame_count += 1;

        let elapsed = start.elapsed().as_secs_f64();
        let frame_bytes = array.data().as_u8_slice().len();
        if self.cfg.store_performance {
            self.total_runtime += elapsed;
            self.total_bytes += frame_bytes as u64;
            self.record_performance(elapsed, frame_bytes);
        }
        Ok(())
    }

    /// C `readFile` returns asynError (NDFileHDF5.cpp:1634-1638).
    fn read_file(&mut self, _pool: &NDArrayPool) -> ADResult<NDArray> {
        Err(ADError::UnsupportedConversion(
            "NDFileHDF5 cannot read".into(),
        ))
    }

    /// Close the open file.
    ///
    /// Every step below is fallible and each one runs against the open handle,
    /// so the release is owned by a finalizer rather than by the tail of the
    /// function: dropping `handle` closes the H5 file id and its dataset ids on
    /// every exit path, and HDF5 finalises the file on that drop. Without it a
    /// failing flush leaked one file id per attempt and left the file
    /// unfinalised under its `.tmp` name.
    fn close_file(&mut self) -> ADResult<()> {
        self.read_settings();
        let mut closing = Finalize::new(self, |w: &mut Self| {
            w.handle = None;
            w.current_path = None;
        });
        closing.run(|w| {
            match w.handle {
                Some(Hdf5Handle::Standard { .. }) => {
                    // Flush each detector dataset's partial frame band and trim its
                    // extent, then emit the accumulated attribute and performance
                    // datasets before the file is finalised.
                    w.finalize_standard_datasets()?;
                    // C `writeAttributeDataset(OnFileClose)` (NDFileHDF5.cpp:1661-1662).
                    if w.cfg.store_attributes {
                        write_attribute_datasets(
                            &mut w.attr_datasets,
                            &w.file_attributes,
                            LayoutWhen::OnFileClose,
                            None,
                            None,
                        )?;
                    }
                    pad_unwritten_attribute_datasets(&mut w.attr_datasets, None)?;
                    w.flush_attribute_datasets()?;
                    w.flush_performance_dataset()?;
                    // Materialise layout `<hardlink>` elements last, once every
                    // dataset a link may target exists on disk.
                    match w.handle {
                        Some(Hdf5Handle::Standard {
                            ref file,
                            ref detectors,
                        }) => {
                            w.build_layout_hardlinks(file)?;
                            // Every group and dataset now exists, so layout
                            // `<attribute source="ndattribute">` element-attrs can
                            // be attached with their open/close values (ADP-79).
                            w.flush_ndattr_element_attrs(file, detectors)?;
                        }
                        _ => unreachable!("handle is Standard in this arm"),
                    }
                    // Finalize the file. Dropping the `H5File` finalizes durably
                    // (with fsync); when `AD_HDF5_FSYNC_ON_CLOSE` opts out, use the
                    // no-fsync fast close instead (rust-hdf5 0.3.2 `close_no_sync`).
                    // Detector dataset handles are released only after finalize,
                    // preserving the prior file-before-datasets drop order.
                    if let Some(Hdf5Handle::Standard { file, detectors }) = w.handle.take() {
                        if w.fsync_on_close {
                            drop(file);
                        } else {
                            file.close_no_sync().map_err(|e| {
                                ADError::UnsupportedConversion(format!(
                                    "HDF5 close_no_sync error: {}",
                                    e
                                ))
                            })?;
                        }
                        drop(detectors);
                    }
                }
                Some(Hdf5Handle::Swmr { .. }) => {
                    // The layout group tree, the nested dataset placement and the
                    // layout `<hardlink>` elements were all materialised in
                    // `open_swmr` before `start_swmr()` (C `NDFileHDF5.cpp:319`-
                    // `325`: `createHardLinks` then `startSWMR`), so SWMR readers
                    // see them for the whole streaming window. Close adds what
                    // C writes at close: the OnFileClose values and the
                    // performance rows.
                    if let Some(Hdf5Handle::Swmr {
                        mut writer,
                        ds_index,
                        grid,
                        perf,
                        ..
                    }) = w.handle.take()
                    {
                        if w.cfg.store_attributes {
                            write_attribute_datasets(
                                &mut w.attr_datasets,
                                &w.file_attributes,
                                LayoutWhen::OnFileClose,
                                None,
                                Some(writer.as_mut()),
                            )?;
                        }
                        pad_unwritten_attribute_datasets(
                            &mut w.attr_datasets,
                            Some(writer.as_mut()),
                        )?;
                        // C `storeOnCloseAttributes` (NDFileHDF5.cpp:1663).
                        w.write_swmr_ndattr_element_attrs(&mut writer, true)?;
                        // C's detector dataset starts at extent 1 too
                        // (NDFileHDF5Dataset.cpp:89).
                        if w.frame_count == 0 && grid.is_none() {
                            let data_type = w.open_data_type.ok_or_else(|| {
                                ADError::UnsupportedConversion("dataset not initialised".into())
                            })?;
                            let fill = widen_le(
                                ad_core_rs::convert::c_cast_f64_le(w.cfg.fill_value, data_type),
                                data_type,
                                w.stored_elem_size,
                            );
                            let elements: usize = w.open_frame_dims.iter().flatten().product();
                            writer
                                .append_frame(ds_index, &fill.repeat(elements))
                                .map_err(|e| {
                                    ADError::UnsupportedConversion(format!(
                                        "SWMR append error: {e}"
                                    ))
                                })?;
                        }
                        // C `writePerformanceDataset` (NDFileHDF5.cpp:1666,
                        // :2677-2695): every row, then the extent to the rows.
                        if let Some(index) = perf.filter(|_| w.cfg.store_performance) {
                            let err = |e: rust_hdf5::Hdf5Error| {
                                ADError::UnsupportedConversion(format!(
                                    "SWMR performance write error: {e}"
                                ))
                            };
                            for (c, chunk) in w.performance_chunks().iter().enumerate() {
                                writer
                                    .write_chunk_at(index, &[c as u64, 0], chunk)
                                    .map_err(err)?;
                            }
                            writer
                                .set_extent(index, &[w.perf_rows.len() as u64, 5])
                                .map_err(err)?;
                        }
                        // Commit any chunk writes made since the last periodic flush
                        // before closing. `SwmrFileWriter::close` finalizes the
                        // extensible-array index of the `append_frame` path but does
                        // not flush the fixed-array index that the grid path's
                        // `write_chunk_at` records, so a grid file's tail frames
                        // would otherwise read back as fill.
                        flush_swmr_writer(&mut w.attr_datasets, &mut writer)?;
                        writer.close().map_err(|e| {
                            ADError::UnsupportedConversion(format!("SWMR close error: {}", e))
                        })?;
                    }
                }
                None => {}
            }
            Ok(())
        })
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
    pos_running: Option<usize>,
    pos_name: [Option<usize>; MAX_EXTRA_DIMS],
    pos_index_name: [Option<usize>; MAX_EXTRA_DIMS],
    extra_dim_size: [Option<usize>; MAX_EXTRA_DIMS],
    extra_dim_name: [Option<usize>; MAX_EXTRA_DIMS],
    extra_dim_chunk: [Option<usize>; MAX_EXTRA_DIMS],
    fill_value: Option<usize>,
    dim_att_datasets: Option<usize>,
    layout_filename: Option<usize>,
    layout_valid: Option<usize>,
    layout_error_msg: Option<usize>,
}

/// Why C `verifyLayoutXMLFile` (NDFileHDF5.cpp:2138-2181) refuses an
/// XMLFileName.
#[derive(Clone, Copy, Debug, PartialEq)]
enum LayoutFault {
    CannotOpen,
    ParserError,
}

impl LayoutFault {
    /// What C sets LayoutErrorMsg to.
    fn message(self) -> &'static str {
        match self {
            Self::CannotOpen => "XML description file cannot be opened",
            Self::ParserError => "XML description file parser error",
        }
    }

    /// The line C prints.
    fn line(self) -> &'static str {
        match self {
            Self::CannotOpen => {
                "NDFileHDF5::verifyLayoutXMLFile XML description file could not be opened.\n"
            }
            Self::ParserError => {
                "NDFileHDF5::verifyLayoutXMLFile XML description file parser error.\n"
            }
        }
    }
}

/// C `verifyLayoutXMLFile`: the LayoutErrorMsg of an XMLFileName it accepts,
/// or why it refuses one. `writeOctet` and `openFile` both run it.
fn verify_layout(spec: &str) -> Result<&'static str, LayoutFault> {
    if spec.is_empty() {
        return Ok("Default layout selected");
    }
    if !Hdf5Layout::is_inline(spec) && !Path::new(spec).exists() {
        return Err(LayoutFault::CannotOpen);
    }
    Hdf5Layout::verify(spec)
        .map(|_| "")
        .map_err(|_| LayoutFault::ParserError)
}

/// What C `openFile` sets and prints for one open, held for the port: the
/// params it posts and its `ASYN_TRACE_ERROR` lines.
#[derive(Debug, Default)]
struct OpenPosts {
    posts: Vec<OpenPost>,
    lines: Vec<String>,
}

/// A param C `openFile` sets.
#[derive(Clone, Copy, Debug, PartialEq)]
enum OpenPost {
    /// The flush counter, zeroed on every open (NDFileHDF5.cpp:185).
    SwmrCbCounter(i32),
    /// `verifyLayoutXMLFile`'s LayoutValid and LayoutErrorMsg (:213).
    Layout(bool, &'static str),
    Compression(CompressionParam, i32),
}

/// HDF5 file processor wrapping `FilePluginController<Hdf5Writer>`.
pub struct Hdf5FileProcessor {
    ctrl: Mutex<FilePluginController<Hdf5Writer>>,
    /// The writer's settings store, written without the writer's lock.
    settings: SettingsStore<Hdf5Settings>,
    /// The writer's `open_posts`, drained without the writer's lock.
    open_posts: Arc<Mutex<OpenPosts>>,
    hdf5_params: Hdf5ParamIndices,
    attributes: Option<PluginAttributes>,
}

impl Hdf5FileProcessor {
    /// The params the writer's opens set, posted as C's `setIntegerParam`
    /// in `openFile` posts them.
    fn open_updates(&self) -> Vec<ParamUpdate> {
        let p = &self.hdf5_params;
        let posts = std::mem::take(&mut self.open_posts.lock().posts);
        posts
            .into_iter()
            .flat_map(|post| {
                let int = |idx: Option<usize>, v: i32| idx.map(|i| ParamUpdate::int32(i, v));
                match post {
                    OpenPost::SwmrCbCounter(v) => [int(p.swmr_cb_counter, v), None],
                    OpenPost::Layout(valid, msg) => [
                        int(p.layout_valid, valid as i32),
                        p.layout_error_msg
                            .map(|i| ParamUpdate::octet(i, msg.to_owned())),
                    ],
                    OpenPost::Compression(param, v) => {
                        let idx = match param {
                            CompressionParam::Type => p.compression_type,
                            CompressionParam::ZLevel => p.z_compress_level,
                            CompressionParam::BloscLevel => p.blosc_compress_level,
                            CompressionParam::BloscShuffle => p.blosc_shuffle_type,
                            CompressionParam::BloscCompressor => p.blosc_compressor,
                            CompressionParam::NbitOffset => p.nbit_offset,
                            CompressionParam::NbitPrecision => p.nbit_precision,
                        };
                        [int(idx, v), None]
                    }
                }
            })
            .flatten()
            .collect()
    }

    /// The lines the open printed, as C's `asynPrint`.
    fn open_trace(&self) -> Vec<(TraceMask, String)> {
        let lines = std::mem::take(&mut self.open_posts.lock().lines);
        lines.into_iter().map(|l| (TraceMask::ERROR, l)).collect()
    }

    pub fn new() -> Self {
        let writer = Hdf5Writer::new();
        Self {
            settings: writer.settings(),
            open_posts: writer.open_posts(),
            ctrl: Mutex::new(FilePluginController::new(writer)),
            hdf5_params: Hdf5ParamIndices::default(),
            attributes: None,
        }
    }

    pub fn set_dataset_name(&mut self, name: &str) {
        self.ctrl.lock().writer.lock().set_dataset_name(name);
    }

    /// The capture count `validate_int32` stored for the grid (C
    /// `calcNumFrames`, NDFileHDF5.cpp:2438-2459), mirrored into the writer.
    fn calc_num_frames(&self, ctrl: &mut FilePluginController<Hdf5Writer>) {
        let frames: i32 = self.settings.get(|s| {
            let n = s.n_extra_dims;
            if n == 0 {
                return 0;
            }
            s.extra_dims[..=n].iter().map(|d| d.size as i32).product()
        });
        if frames != 0 {
            ctrl.file_base.set_num_capture(frames);
        }
    }

    /// C `NDFileHDF5::writeInt32` (NDFileHDF5.cpp:1764-2076): settings that
    /// shape the file are refused while one is open, out-of-range values are
    /// refused, and a refusal restores the old value and prints C's error line.
    fn check_int32(
        &self,
        reason: usize,
        addr: i32,
        value: i32,
        params: &asyn_rs::port::PortDriverBase,
    ) -> Option<WriteOutcome<i32>> {
        // C hands every write it has no branch for to NDPluginFile::writeInt32
        // (NDFileHDF5.cpp:2059): `None`.
        let name = params.params.param_name(reason)?;
        // C's `this->file != 0`.
        let open = self.ctrl.lock().file_base.is_open();
        let int = |n: &str| {
            params
                .find_param(n)
                .and_then(|i| params.get_int32_param(i, 0).ok())
                .unwrap_or(0)
        };
        let old = params.get_int32_param(reason, addr).unwrap_or(0);
        let refuse = |keep: i32| {
            // C sets no errorMessage, so devAsyn's line ends in "error ".
            WriteOutcome::from(WriteVerdict::Refuse {
                keep,
                message: String::new(),
            })
            .settled()
            .print(
                TraceMask::ERROR,
                format!(
                    "NDFileHDF5:writeInt32: ERROR status=3, function={reason}, value={value} old={old}\n"
                ),
            )
        };
        let refused = match name {
            "HDF5_nExtraDims" => open || !(0..MAX_EXTRA_DIMS as i32).contains(&value),
            "NUM_CAPTURE" => {
                let n = int("HDF5_nExtraDims");
                if value < 0 {
                    true
                } else if value > 0 && n > 0 {
                    let frames = (0..=n as usize)
                        .map(|i| int(EXTRA_DIM_SIZE_PARAMS[i]))
                        .product::<i32>();
                    return Some(WriteVerdict::Accept(value.min(frames)).into());
                } else {
                    false
                }
            }
            "HDF5_nColChunks" | "HDF5_nRowChunks" | "HDF5_nFramesChunks" => open || value < 0,
            n if n.starts_with("HDF5_chunkSize") && n != "HDF5_chunkSizeAuto" => open || value < 0,
            "HDF5_flushNthFrame" => value < int("HDF5_nFramesChunks"),
            n if n.starts_with("HDF5_extraDimSize") => open || value <= 0,
            n if n.starts_with("HDF5_extraDimChunk") => open || value < 0,
            "HDF5_storeAttributes" | "HDF5_storePerformance" => open,
            // HDF5Compression_t (NDFileHDF5.cpp:36-44) ends at JPEG = 8.
            "HDF5_compressionType" => open || !(0..=8).contains(&value),
            "HDF5_nbitsPrecision" | "HDF5_nbitsOffset" => open || value < 0,
            "HDF5_szipNumPixels" => open || !(0..=32).contains(&value),
            "HDF5_zCompressLevel" => open || !(0..=9).contains(&value),
            // Read-only: the write fails and the param shows the support.
            "HDF5_SWMRSupported" => return Some(refuse(1)),
            // :2042 stores 1; the flush clears it (:398).
            "HDF5_SWMRFlushNow" if open && value > 0 => {
                return Some(WriteVerdict::Accept(1).into());
            }
            "HDF5_SWMRFlushNow" => true,
            "HDF5_posRunning" => false,
            _ => return None,
        };
        if refused {
            return Some(refuse(old));
        }
        // calcNumFrames (NDFileHDF5.cpp:2438-2459) over the params as they
        // stand once this write is stored.
        let grid = |n: i32, size: Option<(usize, i32)>| {
            (0..=n.max(0) as usize)
                .map(|i| match size {
                    Some((k, v)) if k == i => v,
                    _ => int(EXTRA_DIM_SIZE_PARAMS[i]),
                })
                .product::<i32>()
        };
        let set = |out: WriteOutcome<i32>, n: &str, v: i32| match params.find_param(n) {
            Some(i) => out.set_int32(i, 0, v),
            None => out,
        };
        let out = WriteOutcome::from(WriteVerdict::Accept(value));
        let size = EXTRA_DIM_SIZE_PARAMS.iter().position(|p| *p == name);
        Some(match name {
            "HDF5_nExtraDims" if value > 0 => set(out, "NUM_CAPTURE", grid(value, None)),
            _ if size.is_some() && int("HDF5_nExtraDims") > 0 => set(
                out,
                "NUM_CAPTURE",
                grid(int("HDF5_nExtraDims"), size.map(|k| (k, value))),
            ),
            // An unbounded capture cannot use the fixed grid unless positions
            // place the frames (:1814, :1834).
            "NUM_CAPTURE" if value == 0 && int("HDF5_posRunning") == 0 => {
                set(out, "HDF5_nExtraDims", 0)
            }
            "HDF5_posRunning" if value == 0 && int("NUM_CAPTURE") == 0 => {
                set(out, "HDF5_nExtraDims", 0)
            }
            _ => out,
        })
    }

    /// C `NDFileHDF5::writeOctet` (NDFileHDF5.cpp:2086-2119) keeps an
    /// XMLFileName that `verifyLayoutXMLFile` (:2138-2181) rejects and fails
    /// the write, after the console line naming why; LayoutValid and
    /// LayoutErrorMsg are set inside the write either way.
    fn check_octet(
        &self,
        reason: usize,
        _addr: i32,
        value: &str,
        verdict: WriteVerdict<String>,
        _params: &asyn_rs::port::PortDriverBase,
    ) -> WriteOutcome<String> {
        if Some(reason) != self.hdf5_params.layout_filename {
            return verdict.into();
        }
        let p = &self.hdf5_params;
        let verified = |out: WriteOutcome<String>, valid: bool, msg: &str| {
            let out = match p.layout_valid {
                Some(i) => out.set_int32(i, 0, valid as i32),
                None => out,
            };
            match p.layout_error_msg {
                Some(i) => out.set_octet(i, 0, msg),
                None => out,
            }
        };
        let fault = match verify_layout(value) {
            Ok(msg) => return verified(verdict.into(), true, msg),
            Err(fault) => fault,
        };
        verified(
            WriteOutcome::from(WriteVerdict::Refuse {
                keep: value.to_owned(),
                message: format!(
                    "NDFileHDF5:writeOctet: status=3, function={reason}, value={value}"
                ),
            }),
            false,
            fault.message(),
        )
        .print(TraceMask::ERROR, fault.line())
    }

    /// The writer half of C `NDFileHDF5::writeInt32`/`writeFloat64`/
    /// `writeOctet`: the kept value reaches the writer inside the write, so
    /// a Capture or WriteFile written after it opens the file with it.
    fn store(&self, reason: usize, value: &ParamChangeValue, params: &PortDriverBase) {
        let mut ctrl = self.ctrl.lock();
        // -- compression params --
        if Some(reason) == self.hdf5_params.compression_type {
            self.settings
                .update(|s| s.set_compression_type(value.as_i32()));
            return;
        }
        if Some(reason) == self.hdf5_params.z_compress_level {
            self.settings
                .update(|s| s.set_z_compress_level(value.as_i32()));
            return;
        }
        if Some(reason) == self.hdf5_params.szip_num_pixels {
            self.settings
                .update(|s| s.set_szip_num_pixels(value.as_i32() as u32));
            return;
        }
        if Some(reason) == self.hdf5_params.blosc_shuffle_type {
            self.settings
                .update(|s| s.set_blosc_shuffle_type(value.as_i32()));
            return;
        }
        if Some(reason) == self.hdf5_params.blosc_compressor {
            self.settings
                .update(|s| s.set_blosc_compressor(value.as_i32()));
            return;
        }
        if Some(reason) == self.hdf5_params.blosc_compress_level {
            self.settings
                .update(|s| s.set_blosc_compress_level(value.as_i32()));
            return;
        }
        if Some(reason) == self.hdf5_params.nbit_precision {
            self.settings
                .update(|s| s.set_nbit_precision(value.as_i32() as u32));
            return;
        }
        if Some(reason) == self.hdf5_params.nbit_offset {
            self.settings
                .update(|s| s.set_nbit_offset(value.as_i32() as u32));
            return;
        }
        if Some(reason) == self.hdf5_params.jpeg_quality {
            self.settings
                .update(|s| s.set_jpeg_quality(value.as_i32() as u32));
            return;
        }
        if Some(reason) == self.hdf5_params.store_attributes {
            self.settings
                .update(|s| s.set_store_attributes(value.as_i32() != 0));
            return;
        }
        if Some(reason) == self.hdf5_params.store_performance {
            self.settings
                .update(|s| s.set_store_performance(value.as_i32() != 0));
            return;
        }
        // -- chunking params --
        if Some(reason) == self.hdf5_params.chunk_size_auto {
            self.settings
                .update(|s| s.set_chunk_size_auto(value.as_i32() != 0));
            return;
        }
        if Some(reason) == self.hdf5_params.n_row_chunks {
            self.settings
                .update(|s| s.set_n_row_chunks(value.as_i32().max(0) as usize));
            return;
        }
        if Some(reason) == self.hdf5_params.n_col_chunks {
            self.settings
                .update(|s| s.set_n_col_chunks(value.as_i32().max(0) as usize));
            return;
        }
        if Some(reason) == self.hdf5_params.n_frames_chunks {
            self.settings
                .update(|s| s.set_n_frames_chunks(value.as_i32().max(0) as usize));
            return;
        }
        if Some(reason) == self.hdf5_params.ndattr_chunk {
            // `0` (auto) is valid; only negatives are coerced away.
            self.settings
                .update(|s| s.set_ndattr_chunk(value.as_i32().max(0) as usize));
            return;
        }
        // -- extra dimensions --
        if Some(reason) == self.hdf5_params.n_extra_dims {
            self.settings
                .update(|s| s.set_n_extra_dims(value.as_i32().max(0) as usize));
            self.calc_num_frames(&mut ctrl);
            return;
        }
        for i in 0..MAX_EXTRA_DIMS {
            if Some(reason) == self.hdf5_params.extra_dim_size[i] {
                self.settings
                    .update(|s| s.set_extra_dim_size(i, value.as_i32().max(1) as usize));
                self.calc_num_frames(&mut ctrl);
                return;
            }
            if Some(reason) == self.hdf5_params.extra_dim_name[i] {
                self.settings
                    .update(|s| s.set_extra_dim_name(i, value.as_string().unwrap_or("")));
                return;
            }
            if Some(reason) == self.hdf5_params.extra_dim_chunk[i] {
                self.settings
                    .update(|s| s.set_extra_dim_chunk(i, value.as_i32()));
                return;
            }
            if Some(reason) == self.hdf5_params.pos_name[i] {
                self.settings
                    .update(|s| s.set_pos_name(i, value.as_string().unwrap_or("")));
                return;
            }
            if Some(reason) == self.hdf5_params.pos_index_name[i] {
                self.settings
                    .update(|s| s.set_pos_index_name(i, value.as_string().unwrap_or("")));
                return;
            }
        }
        if Some(reason) == self.hdf5_params.fill_value {
            self.settings.update(|s| s.set_fill_value(value.as_f64()));
            return;
        }
        if Some(reason) == self.hdf5_params.dim_att_datasets {
            self.settings
                .update(|s| s.set_dim_att_datasets(value.as_i32() != 0));
            return;
        }
        // -- layout XML --
        if Some(reason) == self.hdf5_params.layout_filename {
            // The write set LayoutValid/LayoutErrorMsg; the next open loads it.
            let path = value.as_string().unwrap_or("").to_string();
            self.settings.update(|s| s.set_layout_filename(&path));
            return;
        }
        // -- SWMR params --
        if Some(reason) == self.hdf5_params.swmr_mode {
            self.settings
                .update(|s| s.set_swmr_mode(value.as_i32() != 0));
            return;
        }
        if Some(reason) == self.hdf5_params.flush_nth_frame {
            self.settings
                .update(|s| s.set_flush_nth_frame(value.as_i32().max(0) as usize));
            return;
        }
        if Some(reason) == self.hdf5_params.pos_running {
            let running = value.as_i32() != 0;
            self.settings.update(|s| s.set_pos_running(running));
            // NDFileHDF5.cpp:1823-1830
            if !running && ctrl.file_base.num_capture_target() == 0 {
                self.settings.update(|s| s.set_n_extra_dims(0));
            }
            return;
        }
        // NDFileHDF5.cpp:1804-1813: an unbounded capture cannot use the
        // fixed extra-dimension grid unless positions place the frames.
        let pos_running = self
            .hdf5_params
            .pos_running
            .and_then(|i| params.get_int32_param(i, 0).ok())
            .unwrap_or(0);
        if Some(reason) == ctrl.params.num_capture && value.as_i32() == 0 && pos_running == 0 {
            self.settings.update(|s| s.set_n_extra_dims(0));
        }
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
    base.create_param("HDF5_extraDimOffsetX", ParamType::Int32)?;
    base.create_param("HDF5_extraDimOffsetY", ParamType::Int32)?;
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
    base.create_param("HDF5_extraDimChunkN", ParamType::Int32)?;
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

/// `HDF5_chunkSize2..9` (C `str_NDFileHDF5_chunkSize[2..]`).
const CHUNK_SIZE_PARAMS: [&str; 8] = [
    "HDF5_chunkSize2",
    "HDF5_chunkSize3",
    "HDF5_chunkSize4",
    "HDF5_chunkSize5",
    "HDF5_chunkSize6",
    "HDF5_chunkSize7",
    "HDF5_chunkSize8",
    "HDF5_chunkSize9",
];

/// C `str_NDFileHDF5_extraDimChunk`, N first.
const EXTRA_DIM_CHUNK_PARAMS: [&str; MAX_EXTRA_DIMS] = [
    "HDF5_extraDimChunkN",
    "HDF5_extraDimChunkX",
    "HDF5_extraDimChunkY",
    "HDF5_extraDimChunk3",
    "HDF5_extraDimChunk4",
    "HDF5_extraDimChunk5",
    "HDF5_extraDimChunk6",
    "HDF5_extraDimChunk7",
    "HDF5_extraDimChunk8",
    "HDF5_extraDimChunk9",
];

const POS_NAME_PARAMS: [&str; MAX_EXTRA_DIMS] = [
    "HDF5_posNameDimN",
    "HDF5_posNameDimX",
    "HDF5_posNameDimY",
    "HDF5_posNameDim3",
    "HDF5_posNameDim4",
    "HDF5_posNameDim5",
    "HDF5_posNameDim6",
    "HDF5_posNameDim7",
    "HDF5_posNameDim8",
    "HDF5_posNameDim9",
];

const POS_INDEX_PARAMS: [&str; MAX_EXTRA_DIMS] = [
    "HDF5_posIndexDimN",
    "HDF5_posIndexDimX",
    "HDF5_posIndexDimY",
    "HDF5_posIndexDim3",
    "HDF5_posIndexDim4",
    "HDF5_posIndexDim5",
    "HDF5_posIndexDim6",
    "HDF5_posIndexDim7",
    "HDF5_posIndexDim8",
    "HDF5_posIndexDim9",
];

impl NDPluginProcess for Hdf5FileProcessor {
    const ASYN_FLAGS: asyn_rs::port::PortFlags = ad_core_rs::plugin::runtime::ASYN_CANBLOCK;
    // NDFileHDF5.cpp:2269, the same mask for both.
    const INTERFACE_MASK: InterfaceMask = InterfaceMask::GENERIC_POINTER;
    const INTERRUPT_MASK: InterfaceMask = Self::INTERFACE_MASK;
    fn process_array(&self, array: &Arc<NDArray>, _pool: &NDArrayPool) -> ProcessResult {
        // Made before the controller lock: its reads wait on the actor,
        // which takes that lock in its writes.
        let mut post = PortPost::of(self.attributes.as_ref());
        let mut ctrl = self.ctrl.lock();
        let was_swmr = ctrl.writer.lock().is_swmr_active();
        let mut result = ctrl.process_array(array, &mut post);
        result.param_updates.extend(self.open_updates());
        // Printed in openFile, ahead of anything its write reports.
        result.trace.splice(0..0, self.open_trace());
        let is_swmr = ctrl.writer.lock().is_swmr_active();

        // SWMR running status changed. A writeFile abort clears it too; C
        // leaves SWMRRunning at 1 on the file it closed (NDFileHDF5.cpp:1549-1552
        // sets only Capture and WriteFile), a stale readback not copied.
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
                    ctrl.writer.lock().swmr_cb_counter as i32,
                ));
            }
        }

        // Performance stats. One snapshot under one lock: the writer mutex is
        // not reentrant, so a second `writer.lock()` inside the same statement
        // (its first guard still live as a temporary) deadlocks this thread
        // against itself — and, as it holds `ctrl`, the whole port with it.
        let perf = {
            let w = ctrl.writer.lock();
            w.cfg
                .store_performance
                .then(|| (w.total_runtime, w.total_bytes))
        };
        if let Some((total_runtime, total_bytes)) = perf {
            if let Some(idx) = self.hdf5_params.total_runtime {
                result
                    .param_updates
                    .push(ParamUpdate::float64(idx, total_runtime));
            }
            if let Some(idx) = self.hdf5_params.total_io_speed {
                let speed = if total_runtime > 0.0 {
                    total_bytes as f64 / total_runtime / 1_000_000.0
                } else {
                    0.0
                };
                result.param_updates.push(ParamUpdate::float64(idx, speed));
            }
        }

        result
    }

    fn bind_attributes(&mut self, attributes: PluginAttributes) {
        self.attributes = Some(attributes);
    }

    fn bind_pool(&mut self, pool: PluginPool) {
        self.ctrl.get_mut().bind_pool(pool);
    }

    /// C appends `H5get_libversion` (NDFileHDF5.cpp:2388-2392); rust-hdf5 has
    /// no such call, so the library named is its pin, as in `_NCProperties`.
    fn plugin_type(&self) -> &str {
        concat!("NDFileHDF5 ver", env!("RUST_HDF5_PIN"))
    }

    /// C `NDFileHDF5` passes `compressionAware = true` to the file driver
    /// (NDFileHDF5.cpp:2268): it accepts pre-compressed input arrays and writes
    /// their bytes verbatim through a matching HDF5 filter via direct chunk
    /// write, rather than having the framework decompress them first. The
    /// standard (non-SWMR) write path implements this; see `write_standard`.
    fn compression_aware(&self) -> bool {
        true
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
        self.ctrl.lock().register_params(base)?;
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
        self.hdf5_params.pos_running = base.find_param("HDF5_posRunning");
        for i in 0..MAX_EXTRA_DIMS {
            self.hdf5_params.extra_dim_size[i] = base.find_param(EXTRA_DIM_SIZE_PARAMS[i]);
            self.hdf5_params.extra_dim_name[i] = base.find_param(EXTRA_DIM_NAME_PARAMS[i]);
            self.hdf5_params.extra_dim_chunk[i] = base.find_param(EXTRA_DIM_CHUNK_PARAMS[i]);
            self.hdf5_params.pos_name[i] = base.find_param(POS_NAME_PARAMS[i]);
            self.hdf5_params.pos_index_name[i] = base.find_param(POS_INDEX_PARAMS[i]);
        }
        self.hdf5_params.fill_value = base.find_param("HDF5_fillValue");
        self.hdf5_params.dim_att_datasets = base.find_param("HDF5_dimAttDatasets");
        self.hdf5_params.layout_filename = base.find_param("HDF5_layoutFilename");
        self.hdf5_params.layout_valid = base.find_param("HDF5_layoutValid");
        self.hdf5_params.layout_error_msg = base.find_param("HDF5_layoutErrorMsg");

        // C's constructor defaults (NDFileHDF5.cpp:2321-2383). The PINI
        // outputs read these back at init and write them again, so an unset
        // or zero param here is written as 0 at boot and refused
        // (extraDimSize must be > 0).
        let int32: [(&str, i32); 27] = [
            ("HDF5_chunkSizeAuto", 1),
            ("HDF5_extraDimOffsetX", 0),
            ("HDF5_extraDimOffsetY", 0),
            ("HDF5_nFramesChunks", 0),
            ("HDF5_NDAttributeChunk", 0),
            ("HDF5_chunkBoundaryAlign", 0),
            ("HDF5_chunkBoundaryThreshold", 65536),
            ("HDF5_nExtraDims", 0),
            ("HDF5_storeAttributes", 1),
            ("HDF5_storePerformance", 1),
            ("HDF5_flushNthFrame", 0),
            ("HDF5_compressionType", 0),
            ("HDF5_nbitsPrecision", 8),
            ("HDF5_nbitsOffset", 0),
            ("HDF5_szipNumPixels", 16),
            ("HDF5_zCompressLevel", 6),
            ("HDF5_bloscShuffleType", 1),
            ("HDF5_bloscCompressor", 0),
            ("HDF5_bloscCompressLevel", 5),
            ("HDF5_dimAttDatasets", 0),
            ("HDF5_jpegQuality", 90),
            ("HDF5_layoutValid", 1),
            ("HDF5_posRunning", 0),
            ("HDF5_SWMRFlushNow", 0),
            ("HDF5_SWMRCbCounter", 0),
            ("HDF5_SWMRMode", 0),
            ("HDF5_SWMRRunning", 0),
        ];
        let chunk_sizes = ["HDF5_nRowChunks", "HDF5_nColChunks"]
            .into_iter()
            .chain(CHUNK_SIZE_PARAMS)
            .map(|n| (n, 0));
        let extra_dims = EXTRA_DIM_SIZE_PARAMS
            .iter()
            .map(|&n| (n, 1))
            .chain(EXTRA_DIM_CHUNK_PARAMS.iter().map(|&n| (n, 0)));
        for (name, value) in int32.into_iter().chain(chunk_sizes).chain(extra_dims) {
            if let Some(idx) = base.find_param(name) {
                base.set_int32_param(idx, 0, value)?;
            }
        }
        for name in ["HDF5_totalRuntime", "HDF5_totalIoSpeed", "HDF5_fillValue"] {
            if let Some(idx) = base.find_param(name) {
                base.set_float64_param(idx, 0, 0.0)?;
            }
        }
        let empty = ["HDF5_layoutErrorMsg", "HDF5_layoutFilename"]
            .into_iter()
            .chain(POS_NAME_PARAMS)
            .chain(POS_INDEX_PARAMS)
            .map(|n| (n, String::new()));
        let dim_names = EXTRA_DIM_NAME_PARAMS
            .iter()
            .enumerate()
            .map(|(i, &n)| (n, format!("scan dimension {i}")));
        for (name, value) in empty.chain(dim_names) {
            if let Some(idx) = base.find_param(name) {
                base.set_string_param(idx, 0, value)?;
            }
        }

        // Report SWMR as always supported
        if let Some(idx) = self.hdf5_params.swmr_supported {
            base.set_int32_param(idx, 0, 1)?;
        }
        Ok(())
    }

    fn posts_before_write(&self, reason: usize, value: i32) -> bool {
        self.ctrl.lock().posts_before_write(reason, value)
    }

    fn processing_required(&self, array: &NDArray) -> bool {
        self.ctrl.lock().processing_required(array)
    }

    fn validate_int32(
        &self,
        reason: usize,
        addr: i32,
        value: i32,
        verdict: WriteVerdict<i32>,
        params: &mut WritePort<'_>,
    ) -> WriteOutcome<i32> {
        let out = match self.check_int32(reason, addr, value, params) {
            Some(out) => out,
            None => self
                .ctrl
                .lock()
                .validate_int32(reason, value, verdict, params),
        };
        let mut out = out.set_updates(self.open_updates());
        out.trace.splice(0..0, self.open_trace());
        if out.settled {
            return out;
        }
        let kept = out.verdict.kept();
        let out = self.ctrl.lock().store_int32(reason, out);
        self.store(reason, &ParamChangeValue::Int32(kept), params);
        out
    }

    fn validate_float64(
        &self,
        reason: usize,
        _addr: i32,
        _value: f64,
        verdict: WriteVerdict<f64>,
        params: &PortDriverBase,
    ) -> WriteOutcome<f64> {
        self.store(reason, &ParamChangeValue::Float64(verdict.kept()), params);
        verdict.into()
    }

    fn validate_octet(
        &self,
        reason: usize,
        addr: i32,
        value: &str,
        verdict: WriteVerdict<String>,
        params: &PortDriverBase,
    ) -> WriteOutcome<String> {
        let out = self.check_octet(reason, addr, value, verdict, params);
        if !out.settled {
            let kept = out.verdict.kept();
            self.ctrl.lock().store_octet(reason, &kept);
            self.store(reason, &ParamChangeValue::Octet(kept), params);
        }
        out
    }

    fn on_param_change(&self, reason: usize, params: &PluginParamSnapshot) -> ParamChangeResult {
        let ctrl = self.ctrl.lock();
        if Some(reason) == self.hdf5_params.swmr_flush_now {
            if params.value.as_i32() != 0 {
                ctrl.writer.lock().flush_swmr();
                let mut updates = vec![];
                if let Some(idx) = self.hdf5_params.swmr_cb_counter {
                    updates.push(ParamUpdate::int32(
                        idx,
                        ctrl.writer.lock().swmr_cb_counter as i32,
                    ));
                }
                // C flushTask clears the request once flushed (:398).
                updates.push(ParamUpdate::int32(reason, 0));
                return ParamChangeResult::updates(updates);
            }
        }
        ParamChangeResult::empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::opened;
    use ad_core_rs::ndarray::{NDDataViewMut, NDDimension};
    use ad_core_rs::plugin::file_base::NDFileMode;

    /// C names the HDF5 library it links (NDFileHDF5.cpp:2388-2392,
    /// `"NDFileHDF5 ver%d.%d.%d"` from `H5get_libversion`); here that library
    /// is rust-hdf5, at the version the workspace lock resolves.
    #[test]
    fn plugin_type_names_the_linked_hdf5_library() {
        let lock =
            std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/../../Cargo.lock"))
                .expect("workspace Cargo.lock");
        let linked = lock
            .split("[[package]]")
            .find(|p| p.contains("name = \"rust-hdf5\""))
            .and_then(|p| p.lines().find_map(|l| l.strip_prefix("version = \"")))
            .and_then(|v| v.strip_suffix('"'))
            .expect("rust-hdf5 in Cargo.lock");
        assert_eq!(
            Hdf5FileProcessor::new().plugin_type(),
            format!("NDFileHDF5 ver{linked}")
        );
    }
    use ad_core_rs::attributes::{NDAttrSource, NDAttrValue, NDAttribute};
    use rust_hdf5::format::messages::filter::FILTER_LZ4;
    use std::sync::atomic::{AtomicU32, Ordering};

    static TEST_COUNTER: AtomicU32 = AtomicU32::new(0);

    #[test]
    fn test_register_params_seeds_the_c_constructor_values() {
        // Every value NDFileHDF5.cpp:2321-2384 sets, written out from the C.
        use asyn_rs::port::InterfaceMask;
        use asyn_rs::port::{PortDriverBase, PortFlags};
        let mut base = PortDriverBase::new(
            "H5INIT",
            1,
            PortFlags::default(),
            InterfaceMask::UNMIGRATED_ALL,
            InterfaceMask::UNMIGRATED_INTERRUPTS,
        );
        let _nd = ad_core_rs::params::ndarray_driver::NDArrayDriverParams::create(&mut base);
        let mut proc = Hdf5FileProcessor::new();
        proc.register_params(&mut base).unwrap();
        let idx = |name: &str| base.find_param(name).unwrap();
        let int = |name: &str| base.get_int32_param(idx(name), 0).unwrap();
        let float = |name: &str| base.get_float64_param(idx(name), 0).unwrap();
        let string = |name: &str| {
            String::from_utf8(base.get_string_param(idx(name), 0).unwrap().to_vec()).unwrap()
        };
        let ints = [
            ("HDF5_chunkSizeAuto", 1),
            ("HDF5_nRowChunks", 0),
            ("HDF5_nColChunks", 0),
            ("HDF5_chunkSize2", 0),
            ("HDF5_chunkSize3", 0),
            ("HDF5_chunkSize4", 0),
            ("HDF5_chunkSize5", 0),
            ("HDF5_chunkSize6", 0),
            ("HDF5_chunkSize7", 0),
            ("HDF5_chunkSize8", 0),
            ("HDF5_chunkSize9", 0),
            ("HDF5_nFramesChunks", 0),
            ("HDF5_NDAttributeChunk", 0),
            ("HDF5_chunkBoundaryAlign", 0),
            ("HDF5_chunkBoundaryThreshold", 65536),
            ("HDF5_nExtraDims", 0),
            ("HDF5_extraDimOffsetX", 0),
            ("HDF5_extraDimOffsetY", 0),
            ("HDF5_storeAttributes", 1),
            ("HDF5_storePerformance", 1),
            ("HDF5_flushNthFrame", 0),
            ("HDF5_compressionType", 0),
            ("HDF5_nbitsPrecision", 8),
            ("HDF5_nbitsOffset", 0),
            ("HDF5_szipNumPixels", 16),
            ("HDF5_zCompressLevel", 6),
            ("HDF5_bloscShuffleType", 1),
            ("HDF5_bloscCompressor", 0),
            ("HDF5_bloscCompressLevel", 5),
            ("HDF5_dimAttDatasets", 0),
            ("HDF5_jpegQuality", 90),
            ("HDF5_layoutValid", 1),
            ("HDF5_posRunning", 0),
            ("HDF5_SWMRFlushNow", 0),
            ("HDF5_SWMRCbCounter", 0),
            ("HDF5_SWMRMode", 0),
            ("HDF5_SWMRRunning", 0),
            ("HDF5_SWMRSupported", 1),
        ];
        for (name, v) in ints {
            assert_eq!(int(name), v, "{name}");
        }
        for name in ["HDF5_totalRuntime", "HDF5_totalIoSpeed", "HDF5_fillValue"] {
            assert_eq!(float(name), 0.0, "{name}");
        }
        assert_eq!(string("HDF5_layoutErrorMsg"), "");
        assert_eq!(string("HDF5_layoutFilename"), "");
        let dims = ["N", "X", "Y", "3", "4", "5", "6", "7", "8", "9"];
        for (i, d) in dims.iter().enumerate() {
            assert_eq!(int(&format!("HDF5_extraDimSize{d}")), 1, "size {d}");
            assert_eq!(int(&format!("HDF5_extraDimChunk{d}")), 0, "chunk {d}");
            assert_eq!(
                string(&format!("HDF5_extraDimName{d}")),
                format!("scan dimension {i}")
            );
            assert_eq!(string(&format!("HDF5_posNameDim{d}")), "");
            assert_eq!(string(&format!("HDF5_posIndexDim{d}")), "");
        }
    }

    fn temp_path(prefix: &str) -> PathBuf {
        let n = TEST_COUNTER.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "adcore_test_{}_{}_{}.h5",
            std::process::id(),
            prefix,
            n
        ))
    }

    /// C `openFile` refuses NDFileModeRead (NDFileHDF5.cpp:192), so ReadFile
    /// fails at the open.
    #[test]
    fn read_file_is_refused_at_the_open() {
        let path = temp_path("hdf5_read_refused");
        let mut writer = Hdf5Writer::new();
        let arr = NDArray::new(
            vec![NDDimension::new(4), NDDimension::new(4)],
            NDDataType::UInt8,
        );
        writer
            .open_file(&path, NDFileOpenMode::WRITE, &arr)
            .unwrap();
        writer.write_file(&Arc::new(arr)).unwrap();
        writer.close_file().unwrap();
        let mut proc = Hdf5FileProcessor::new();
        let (out, _base) = crate::test_util::read_file(&mut proc, &path);
        assert_eq!(out.trace, crate::test_util::read_refused_lines(&path));
        assert_eq!(out.publish, ad_core_rs::plugin::runtime::Published::Nothing);
        // C also drops Capture and WriteFile (NDFileHDF5.cpp:192-196).
        let posted = |name: &str| {
            let reason = _base.find_param(name).unwrap();
            out.side.iter().rev().find_map(|s| match s {
                ad_core_rs::plugin::runtime::SideParam::Int32 {
                    reason: r, value, ..
                } if *r == reason => Some(*value),
                _ => None,
            })
        };
        assert_eq!(
            (posted("CAPTURE"), posted("WRITE_FILE")),
            (Some(0), Some(0))
        );
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn test_write_single_frame() {
        let path = temp_path("hdf5_single");
        let mut writer = Hdf5Writer::new();

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

        // A Single file is the frame alone.
        let h5 = H5File::open(&path).unwrap();
        let ds = h5.dataset("entry/instrument/detector/data").unwrap();
        assert_eq!(ds.shape(), vec![4, 4]);
        let data: Vec<u8> = ds.read_raw().unwrap();
        assert_eq!(data[0], 0);
        assert_eq!(data[15], 15);
        drop(h5);
        std::fs::remove_file(&path).ok();
    }

    fn hdf5_port() -> (Hdf5FileProcessor, asyn_rs::port::PortDriverBase) {
        let mut proc = Hdf5FileProcessor::new();
        let mut base = asyn_rs::port::PortDriverBase::new(
            "HDF5V",
            1,
            Default::default(),
            asyn_rs::port::InterfaceMask::UNMIGRATED_ALL,
            asyn_rs::port::InterfaceMask::UNMIGRATED_INTERRUPTS,
        );
        ad_core_rs::params::ndarray_driver::NDArrayDriverParams::create(&mut base).unwrap();
        proc.register_params(&mut base).unwrap();
        (proc, base)
    }

    fn idx(base: &asyn_rs::port::PortDriverBase, name: &str) -> usize {
        base.find_param(name).unwrap()
    }

    /// The template's PINI outputs read their param back at init and write
    /// it again, so every seeded default they carry must pass its own write
    /// rule (C seeds extraDimSize = 1; a 0 here is refused at boot).
    #[test]
    fn every_pini_output_accepts_its_seeded_default() {
        let (proc, mut base) = hdf5_port();
        let template = include_str!("../../ad-core-rs/db/NDFileHDF5.template");
        let mut refused = Vec::new();
        for record in template.split("\nrecord(").skip(1) {
            let is_output = ["bo,", "mbbo,", "longout,"]
                .iter()
                .any(|t| record.starts_with(t));
            if !is_output || !record.contains(r#"field(PINI, "YES")"#) {
                continue;
            }
            let Some(name) = record
                .lines()
                .find(|l| l.contains("field(OUT"))
                .and_then(|l| l.rsplit(')').nth(1))
                .map(|n| n.trim_end_matches('"'))
            else {
                continue;
            };
            let idx = idx(&base, name);
            let value = base.get_int32_param(idx, 0).unwrap();
            let verdict = proc
                .validate_int32(
                    idx,
                    0,
                    value,
                    WriteVerdict::Accept(value),
                    &mut ad_core_rs::plugin::runtime::WritePort::new(&mut base),
                )
                .verdict;
            if verdict != WriteVerdict::Accept(value) {
                refused.push(format!("{name}={value}"));
            }
        }
        assert!(refused.is_empty(), "refused: {refused:?}");
    }

    #[test]
    fn writes_are_refused_and_reverted_as_c_write_int32_does() {
        let (proc, mut base) = hdf5_port();
        let (zlevel, chunks) = (
            idx(&base, "HDF5_zCompressLevel"),
            idx(&base, "HDF5_nFramesChunks"),
        );
        base.set_int32_param(zlevel, 0, 6).unwrap();
        base.set_int32_param(chunks, 0, 4).unwrap();
        let mut v = |name: &str, value| {
            let mut port = ad_core_rs::plugin::runtime::WritePort::new(&mut base);
            let r = idx(&port, name);
            proc.validate_int32(r, 0, value, WriteVerdict::Accept(value), &mut port)
                .verdict
        };
        let refused = |keep| WriteVerdict::Refuse {
            keep,
            message: String::new(),
        };

        assert_eq!(v("HDF5_zCompressLevel", 9), WriteVerdict::Accept(9));
        assert_eq!(v("HDF5_zCompressLevel", 10), refused(6));
        assert_eq!(v("HDF5_szipNumPixels", 33), refused(16));
        assert_eq!(v("HDF5_nExtraDims", 9), WriteVerdict::Accept(9));
        assert_eq!(v("HDF5_nExtraDims", 10), refused(0));
        assert_eq!(v("HDF5_compressionType", 9), refused(0));
        assert_eq!(v("HDF5_flushNthFrame", 3), refused(0));
        assert_eq!(v("HDF5_flushNthFrame", 4), WriteVerdict::Accept(4));
        assert_eq!(v("HDF5_extraDimSizeX", 0), refused(1));
        assert_eq!(v("HDF5_chunkSize3", -1), refused(0));
        assert_eq!(v("NUM_CAPTURE", -1), refused(0));
        assert_eq!(v("HDF5_SWMRSupported", 0), refused(1));
        assert_eq!(v("HDF5_SWMRFlushNow", 1), refused(0));

        // An open file freezes the layout settings.
        let path = temp_path("hdf5_open_freezes");
        {
            let mut c = proc.ctrl.lock();
            let c = &mut *c;
            c.file_base.file_path = format!("{}/", path.parent().unwrap().display());
            c.file_base.file_name = path.file_name().unwrap().to_string_lossy().into_owned();
            c.file_base.file_template = "%s%s".into();
            c.file_base.set_mode(NDFileMode::Stream);
            let arr = NDArray::new(
                vec![NDDimension::new(4), NDDimension::new(4)],
                NDDataType::UInt8,
            );
            c.file_base
                .open_stream(&mut *c.writer.lock(), &arr)
                .unwrap();
        }
        assert_eq!(v("HDF5_zCompressLevel", 3), refused(6));
        assert_eq!(v("HDF5_nFramesChunks", 8), refused(4));
        assert_eq!(v("HDF5_storeAttributes", 0), refused(1));
        assert_eq!(v("HDF5_SWMRFlushNow", 1), WriteVerdict::Accept(1));
        assert_eq!(v("HDF5_flushNthFrame", 4), WriteVerdict::Accept(4));
        {
            let mut c = proc.ctrl.lock();
            let c = &mut *c;
            let arr = NDArray::new(
                vec![NDDimension::new(4), NDDimension::new(4)],
                NDDataType::UInt8,
            );
            c.file_base
                .process_array(Arc::new(arr), &mut *c.writer.lock())
                .unwrap();
            c.file_base.close_stream(&mut *c.writer.lock()).unwrap();
        }
        std::fs::remove_file(&path).ok();
    }

    /// C `configureCompression` writes the codec it takes from a
    /// pre-compressed frame into the params (NDFileHDF5.cpp:3319-3337), so
    /// the readbacks show it and the next file is built with it.
    #[test]
    fn a_frame_codec_override_is_posted_and_kept() {
        let (proc, mut base) = hdf5_port();
        let dir = std::env::temp_dir().join(format!("h5override_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let octet = |name: &str, v: &str| {
            proc.validate_octet(
                idx(&base, name),
                0,
                v,
                WriteVerdict::Accept(v.to_owned()),
                &base,
            )
        };
        octet("FILE_PATH", &format!("{}/", dir.display()));
        octet("FILE_NAME", "o");
        octet("FILE_TEMPLATE", "%s%s_%d.h5");
        let names = [
            "HDF5_compressionType",
            "HDF5_zCompressLevel",
            "HDF5_bloscCompressLevel",
            "HDF5_bloscShuffleType",
            "HDF5_bloscCompressor",
        ]
        .map(|n| (n, idx(&base, n)));
        let mut int = |name: &str, v: i32| {
            let mut port = ad_core_rs::plugin::runtime::WritePort::new(&mut base);
            let reason = idx(&port, name);
            proc.validate_int32(reason, 0, v, WriteVerdict::Accept(v), &mut port)
        };
        int("WRITE_MODE", 0);
        int("AUTO_SAVE", 1);
        int("AUTO_INCREMENT", 1);
        let r = |name: &str| names.iter().find(|(n, _)| *n == name).unwrap().1;
        let posted = |updates: &[ParamUpdate], name| {
            updates.iter().rev().find_map(|u| match u {
                ParamUpdate::Int32 { reason, value, .. } if *reason == r(name) => Some(*value),
                _ => None,
            })
        };

        let zlib = crate::codec::compress_zlib(&ramp_u16(4, 5), 4).unwrap();
        let out = proc.process_array(&Arc::new(zlib), &NDArrayPool::new(0));
        assert_eq!(posted(&out.param_updates, "HDF5_compressionType"), Some(3));
        assert_eq!(posted(&out.param_updates, "HDF5_zCompressLevel"), Some(4));
        assert_eq!(
            proc.settings
                .get(|s| (s.compression_type, s.z_compress_level)),
            (3, 4),
            "the next file is built with the frame's codec"
        );

        let cfg = crate::codec::BloscConfig {
            compressor: 1,
            clevel: 7,
            shuffle: 2,
        };
        let blosc = crate::codec::compress_blosc(&ramp_u16(4, 5), &cfg).unwrap();
        proc.ctrl.lock().latest_array = Some(Arc::new(blosc));
        let out = int("WRITE_FILE", 1);
        let side = |name| {
            out.side.iter().rev().find_map(|p| match p {
                ad_core_rs::plugin::runtime::SideParam::Int32 { reason, value, .. }
                    if *reason == r(name) =>
                {
                    Some(*value)
                }
                _ => None,
            })
        };
        assert_eq!(side("HDF5_compressionType"), Some(4));
        assert_eq!(side("HDF5_bloscCompressLevel"), Some(7));
        assert_eq!(side("HDF5_bloscShuffleType"), Some(2));
        assert_eq!(side("HDF5_bloscCompressor"), Some(1));

        // C prints the JPEG gate line in openFile, before the refusal.
        let src = NDArray::with_data(
            vec![NDDimension::new(8), NDDimension::new(8)],
            NDDataBuffer::from_vec::<u8>(vec![7; 64]),
        );
        let jpeg = crate::codec::compress_jpeg(&src, 90).unwrap();
        let jpeg = with_color_mode(jpeg, NDColorMode::RGB1);
        let out = proc.process_array(&Arc::new(jpeg), &NDArrayPool::new(0));
        assert_eq!(
            out.trace.first().map(|(_, l)| l.as_str()),
            Some("JPEG compression only supports 2-D mono and 3-D RGB1 modes\n")
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    /// C `openFile` zeroes SWMRCbCounter before it refuses anything
    /// (NDFileHDF5.cpp:185), so after a SWMR run both a plain file and a
    /// ReadFile read 0; the C oracle reads 8, then 0.
    #[test]
    fn every_open_posts_the_swmr_counter_reset() {
        let (proc, mut base) = hdf5_port();
        let dir = std::env::temp_dir().join(format!("h5cbreset_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        for (name, v) in [
            ("FILE_PATH", format!("{}/", dir.display())),
            ("FILE_NAME", "o".to_owned()),
            ("FILE_TEMPLATE", "%s%s.h5".to_owned()),
        ] {
            proc.validate_octet(
                idx(&base, name),
                0,
                &v,
                WriteVerdict::Accept(v.clone()),
                &base,
            );
        }
        let counter = idx(&base, "HDF5_SWMRCbCounter");
        let mut int = |name: &str, v: i32| {
            let mut port = ad_core_rs::plugin::runtime::WritePort::new(&mut base);
            let reason = idx(&port, name);
            proc.validate_int32(reason, 0, v, WriteVerdict::Accept(v), &mut port)
        };
        int("WRITE_MODE", 0);
        int("AUTO_SAVE", 1);
        proc.ctrl.lock().writer.lock().swmr_cb_counter = 8;
        let out = proc.process_array(&Arc::new(ramp_u16(4, 5)), &NDArrayPool::new(0));
        let posted = out.param_updates.iter().rev().find_map(|u| match u {
            ParamUpdate::Int32 { reason, value, .. } if *reason == counter => Some(*value),
            _ => None,
        });
        assert_eq!(posted, Some(0), "plain file");

        proc.ctrl.lock().writer.lock().swmr_cb_counter = 8;
        let out = int("READ_FILE", 1);
        let posted = out.side.iter().rev().find_map(|s| match s {
            ad_core_rs::plugin::runtime::SideParam::Int32 { reason, value, .. }
                if *reason == counter =>
            {
                Some(*value)
            }
            _ => None,
        });
        assert_eq!(posted, Some(0), "ReadFile");
        assert_eq!(proc.ctrl.lock().writer.lock().swmr_cb_counter, 0);
        std::fs::remove_dir_all(&dir).ok();
    }

    /// The template seeds HDF5_storePerformance = 1, so the performance
    /// block runs on every IOC's first frame. It must take the writer lock
    /// once: v0.32.0 took it twice in one statement and deadlocked the
    /// worker against itself, with `ctrl` held, hanging the whole port.
    #[test]
    fn a_frame_with_store_performance_posts_io_speed() {
        let (proc, mut base) = hdf5_port();
        let dir = std::env::temp_dir().join(format!("h5perf_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        for (name, v) in [
            ("FILE_PATH", format!("{}/", dir.display())),
            ("FILE_NAME", "p".to_owned()),
            ("FILE_TEMPLATE", "%s%s.h5".to_owned()),
        ] {
            proc.validate_octet(
                idx(&base, name),
                0,
                &v,
                WriteVerdict::Accept(v.clone()),
                &base,
            );
        }
        let speed = idx(&base, "HDF5_totalIoSpeed");
        let runtime = idx(&base, "HDF5_totalRuntime");
        let mut int = |name: &str, v: i32| {
            let mut port = ad_core_rs::plugin::runtime::WritePort::new(&mut base);
            let reason = idx(&port, name);
            proc.validate_int32(reason, 0, v, WriteVerdict::Accept(v), &mut port)
        };
        int("WRITE_MODE", 0);
        int("AUTO_SAVE", 1);
        int("HDF5_storePerformance", 1);
        let out = proc.process_array(&Arc::new(ramp_u16(4, 5)), &NDArrayPool::new(0));
        let posted = |want: usize| {
            out.param_updates.iter().rev().find_map(|u| match u {
                ParamUpdate::Float64 { reason, value, .. } if *reason == want => Some(*value),
                _ => None,
            })
        };
        assert!(posted(runtime).is_some(), "totalRuntime posted");
        assert!(posted(speed).is_some(), "totalIoSpeed posted");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// C `openFile` runs `verifyLayoutXMLFile` on every open, a read
    /// included, before it returns (NDFileHDF5.cpp:213-217): the C oracle
    /// posts LayoutValid and LayoutErrorMsg and prints the verify line ahead
    /// of the base step's, and a refused layout leaves no file.
    #[test]
    fn every_open_verifies_the_layout_first() {
        const LAYOUT: &str = r#"<?xml version="1.0"?><hdf5_layout><group name="e"><dataset name="d" source="detector" det_default="true"/></group></hdf5_layout>"#;
        const OPEN: &str =
            "NDFileHDF5::verifyLayoutXMLFile XML description file could not be opened.\n";
        const PARSE: &str = "NDFileHDF5::verifyLayoutXMLFile XML description file parser error.\n";
        let (proc, mut base) = hdf5_port();
        let dir = std::env::temp_dir().join(format!("h5verify_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let lay = dir.join("lay.xml");
        std::fs::write(&lay, LAYOUT).unwrap();
        let (valid, msg) = (
            idx(&base, "HDF5_layoutValid"),
            idx(&base, "HDF5_layoutErrorMsg"),
        );
        let sides = |side: &[ad_core_rs::plugin::runtime::SideParam]| {
            use ad_core_rs::plugin::runtime::SideParam;
            let v = side.iter().rev().find_map(|s| match s {
                SideParam::Int32 { reason, value, .. } if *reason == valid => Some(*value),
                _ => None,
            });
            let m = side.iter().rev().find_map(|s| match s {
                SideParam::Octet { reason, value, .. } if *reason == msg => Some(value.clone()),
                _ => None,
            });
            (v, m)
        };
        let updates = |u: &[ParamUpdate]| {
            let v = u.iter().rev().find_map(|p| match p {
                ParamUpdate::Int32 { reason, value, .. } if *reason == valid => Some(*value),
                _ => None,
            });
            let m = u.iter().rev().find_map(|p| match p {
                ParamUpdate::Octet { reason, value, .. } if *reason == msg => Some(value.clone()),
                _ => None,
            });
            (v, m)
        };
        let octet = |base: &asyn_rs::port::PortDriverBase, name: &str, v: &str| {
            proc.validate_octet(
                idx(base, name),
                0,
                v,
                WriteVerdict::Accept(v.to_owned()),
                base,
            )
        };
        octet(&base, "FILE_PATH", &format!("{}/", dir.display()));
        octet(&base, "FILE_TEMPLATE", "%s%s.h5");
        let out = octet(&base, "HDF5_layoutFilename", lay.to_str().unwrap());
        assert_eq!(sides(&out.side), (Some(1), Some(String::new())), "C0");
        let int = |base: &mut asyn_rs::port::PortDriverBase, name: &str, v: i32| {
            let mut port = ad_core_rs::plugin::runtime::WritePort::new(base);
            let reason = idx(&port, name);
            proc.validate_int32(reason, 0, v, WriteVerdict::Accept(v), &mut port)
        };
        int(&mut base, "WRITE_MODE", 0);
        int(&mut base, "AUTO_SAVE", 1);
        let read_line = |n: &str| {
            format!(
                "NDPluginFile::readFileBase Error opening file {}, status=3\n",
                dir.join(n).display()
            )
        };
        let open_line = |n: &str| {
            format!(
                "NDPluginFile::openFileBase Error opening file {}, status=3\n",
                dir.join(n).display()
            )
        };
        let lines =
            |t: &[(TraceMask, String)]| t.iter().map(|(_, l)| l.clone()).collect::<Vec<_>>();
        let name = |base: &asyn_rs::port::PortDriverBase, n: &str| {
            octet(base, "FILE_NAME", n);
        };

        name(&base, "r");
        std::fs::remove_file(&lay).unwrap();
        let out = int(&mut base, "READ_FILE", 1);
        assert_eq!(
            lines(&out.trace),
            [OPEN.to_owned(), read_line("r.h5")],
            "C1"
        );
        let cannot = Some("XML description file cannot be opened".to_owned());
        assert_eq!(sides(&out.side), (Some(0), cannot.clone()), "C1");

        std::fs::write(&lay, LAYOUT).unwrap();
        let out = int(&mut base, "READ_FILE", 1);
        assert_eq!(lines(&out.trace), [read_line("r.h5")], "C2");
        assert_eq!(sides(&out.side), (Some(1), Some(String::new())), "C2");

        std::fs::remove_file(&lay).unwrap();
        name(&base, "nolay");
        let out = proc.process_array(&Arc::new(ramp_u16(4, 5)), &NDArrayPool::new(0));
        assert_eq!(
            lines(&out.trace),
            [OPEN.to_owned(), open_line("nolay.h5")],
            "C3"
        );
        assert_eq!(updates(&out.param_updates), (Some(0), cannot), "C3");
        assert!(!dir.join("nolay.h5").exists(), "C3 leaves no file");

        std::fs::write(&lay, "<hdf5_layout><group").unwrap();
        let out = int(&mut base, "READ_FILE", 1);
        assert_eq!(
            lines(&out.trace),
            [PARSE.to_owned(), read_line("nolay.h5")],
            "C4"
        );
        let parser = Some("XML description file parser error".to_owned());
        assert_eq!(sides(&out.side), (Some(0), parser.clone()), "C4");

        name(&base, "badlay");
        let out = proc.process_array(&Arc::new(ramp_u16(4, 5)), &NDArrayPool::new(0));
        assert_eq!(
            lines(&out.trace),
            [PARSE.to_owned(), open_line("badlay.h5")],
            "C5"
        );
        assert_eq!(updates(&out.param_updates), (Some(0), parser), "C5");
        assert!(!dir.join("badlay.h5").exists(), "C5 leaves no file");

        octet(&base, "HDF5_layoutFilename", "");
        let out = int(&mut base, "READ_FILE", 1);
        assert_eq!(lines(&out.trace), [read_line("badlay.h5")], "C8");
        let default = Some("Default layout selected".to_owned());
        assert_eq!(sides(&out.side), (Some(1), default), "C8");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// C `createAttributeDataset` writes the host's name on the default
    /// NDAttribute group, under StoreAttr only (NDFileHDF5.cpp:2768-2772,
    /// :296); the C oracle file carries `@hostname` there.
    #[test]
    fn the_default_ndattr_group_carries_the_hostname() {
        let host = epics_base_rs::runtime::env::hostname().unwrap();
        let host = String::from_utf8(host).unwrap();
        for (swmr, store) in [(false, true), (true, true), (false, false), (true, false)] {
            let path = temp_path("hdf5_hostname");
            let mut writer = Hdf5Writer::new();
            writer.set_swmr_mode(swmr);
            writer.set_store_attributes(store);
            writer.set_num_capture(0);
            let arr = Arc::new(ramp_u16(4, 5));
            writer
                .open_file(&path, opened(NDFileMode::Stream), &arr)
                .unwrap();
            writer.write_file(&arr).unwrap();
            writer.close_file().unwrap();
            let h5 = H5File::open(&path).unwrap();
            let group = h5
                .root_group()
                .group("entry")
                .and_then(|g| g.group("instrument"))
                .and_then(|g| g.group("NDAttributes"))
                .unwrap();
            let got = group.attr_string("hostname").ok();
            assert_eq!(
                got,
                store.then(|| host.clone()),
                "swmr {swmr} store {store}"
            );
            std::fs::remove_file(&path).ok();
        }
    }

    /// H5Acreate2 refuses a name its object already has, so C keeps the first
    /// value written: a layout @hostname over gethostname's (createTree runs
    /// before createAttributeDataset), gethostname's over a layout ndattribute
    /// one (storeOnOpenAttributes runs after), a layout @NDAttrName over the
    /// descriptor and the descriptor @NDAttrSource over a layout ndattribute
    /// one (NDFileHDF5.cpp:2768-2823). The C oracle files carry each first
    /// value.
    #[test]
    fn an_attribute_keeps_the_first_value_created_under_its_name() {
        let host = String::from_utf8(epics_base_rs::runtime::env::hostname().unwrap()).unwrap();
        for swmr in [false, true] {
            for (hostname_attr, want) in [
                (
                    r#"source="constant" value="LAYOUT" type="string""#,
                    "LAYOUT",
                ),
                (
                    r#"source="ndattribute" ndattribute="AcqT" when="OnFileOpen""#,
                    host.as_str(),
                ),
            ] {
                let path = temp_path("hdf5_first_create");
                let mut writer = Hdf5Writer::new();
                writer.set_swmr_mode(swmr);
                writer.set_layout_filename(&format!(
                    r#"<?xml version="1.0"?><hdf5_layout><group name="entry">
                      <dataset name="data" source="detector" det_default="true"/>
                      <dataset name="acq" source="ndattribute" ndattribute="AcqT">
                        <attribute name="NDAttrName" source="constant" value="mine" type="string"/>
                        <attribute name="NDAttrSource" source="ndattribute" ndattribute="AcqT" when="OnFileOpen"/>
                      </dataset>
                      <group name="NDAttributes" ndattr_default="true">
                        <attribute name="hostname" {hostname_attr}/>
                      </group>
                    </group></hdf5_layout>"#
                ));
                let mut arr = NDArray::new(
                    vec![NDDimension::new(4), NDDimension::new(4)],
                    NDDataType::UInt8,
                );
                arr.attributes.add(NDAttribute::new_static(
                    "AcqT",
                    "Acquire time",
                    NDAttrSource::Param {
                        port_name: "SIM1".into(),
                        param_name: "ACQ_TIME".into(),
                    },
                    NDAttrValue::Float64(0.25),
                ));
                writer
                    .open_file(&path, NDFileOpenMode::WRITE, &arr)
                    .unwrap();
                writer.write_file(&Arc::new(arr)).unwrap();
                writer.close_file().unwrap();

                let h5 = H5File::open(&path).unwrap();
                let ndattrs = h5
                    .root_group()
                    .group("entry")
                    .and_then(|g| g.group("NDAttributes"))
                    .unwrap();
                let got = ndattrs.attr_string("hostname");
                assert_eq!(got.ok().as_deref(), Some(want), "swmr={swmr}");
                if !swmr {
                    let acq = h5.dataset("entry/acq").unwrap();
                    let s = |n: &str| acq.attr(n).and_then(|a| a.read_string()).ok();
                    assert_eq!(s("NDAttrName").as_deref(), Some("mine"));
                    assert_eq!(s("NDAttrSource").as_deref(), Some("ACQ_TIME"));
                }
                std::fs::remove_file(&path).ok();
            }
        }
    }

    /// Every attribute message named `name` in the file at `path`, decoded
    /// from the raw bytes: rust-hdf5 reads a group attribute's value but not
    /// its datatype.
    fn attr_messages(
        path: &Path,
        name: &str,
    ) -> Vec<rust_hdf5::format::messages::attribute::AttributeMessage> {
        use rust_hdf5::format::FormatContext;
        use rust_hdf5::format::messages::attribute::AttributeMessage;
        let bytes = std::fs::read(path).unwrap();
        let key = [name.as_bytes(), b"\0"].concat();
        let ctx = FormatContext::default_v3();
        let mut out = Vec::new();
        for (at, _) in bytes
            .windows(key.len())
            .enumerate()
            .filter(|(_, w)| *w == key)
        {
            // The name follows an 8-byte (version 1/2) or 9-byte (version 3) header.
            let found = [8, 9].into_iter().find_map(|header| {
                let (m, _) =
                    AttributeMessage::decode(&bytes[at.checked_sub(header)?..], &ctx).ok()?;
                (m.name == name).then_some(m)
            });
            out.extend(found);
        }
        out
    }

    /// C writes every string attribute as `H5T_C_S1` sized strlen+1,
    /// NULLTERM, ASCII — writeStringAttribute (NDFileHDF5.cpp:3021-3043) for
    /// the hostname and NDAttr* descriptors, writeH5attrStr (:1074-1100) for
    /// layout constants and string NDAttributes, an empty one included. The C
    /// oracle file carries each as `S<len+1> pad=0 cset=0`.
    #[test]
    fn string_attributes_are_c_s1_nullterm_ascii() {
        let host = String::from_utf8(epics_base_rs::runtime::env::hostname().unwrap()).unwrap();
        let want = |text: &str| {
            (
                c_s1_nullterm(text.len() + 1),
                [text.as_bytes(), b"\0"].concat(),
            )
        };
        for swmr in [false, true] {
            let path = temp_path("hdf5_c_s1_attrs");
            let mut writer = Hdf5Writer::new();
            writer.set_swmr_mode(swmr);
            writer.set_layout_filename(
                r#"<?xml version="1.0"?><hdf5_layout><group name="entry">
                  <attribute name="sgrp" source="constant" value="NXentry" type="string"/>
                  <attribute name="sempty" source="constant" value="" type="string"/>
                  <dataset name="data" source="detector" det_default="true">
                    <attribute name="sdet" source="constant" value="counts" type="string"/>
                    <attribute name="sdyn" source="ndattribute" ndattribute="Man" when="OnFileOpen"/>
                  </dataset>
                  <group name="NDAttributes" ndattr_default="true">
                    <attribute name="sgnd" source="ndattribute" ndattribute="Man" when="OnFileOpen"/>
                  </group>
                </group></hdf5_layout>"#,
            );
            let mut arr = NDArray::new(
                vec![NDDimension::new(4), NDDimension::new(4)],
                NDDataType::UInt8,
            );
            arr.attributes.add(NDAttribute::new_static(
                "Man",
                "Maker",
                NDAttrSource::Param {
                    port_name: "SIM1".into(),
                    param_name: "MANUFACTURER".into(),
                },
                NDAttrValue::String("Simulated detector".into()),
            ));
            writer
                .open_file(&path, NDFileOpenMode::WRITE, &arr)
                .unwrap();
            writer.write_file(&Arc::new(arr)).unwrap();
            writer.close_file().unwrap();

            let mut names = vec![
                ("sgrp", "NXentry"),
                ("sempty", ""),
                ("sdet", "counts"),
                ("sdyn", "Simulated detector"),
                ("sgnd", "Simulated detector"),
                ("hostname", host.as_str()),
            ];
            if !swmr {
                names.extend([
                    ("NDAttrDescription", "Maker"),
                    ("NDAttrSource", "MANUFACTURER"),
                ]);
            }
            for (name, text) in names {
                let got: Vec<_> = attr_messages(&path, name)
                    .into_iter()
                    .map(|m| (m.datatype, m.data))
                    .collect();
                assert!(got.contains(&want(text)), "swmr={swmr} @{name}: {got:?}");
                for (datatype, bytes) in &got {
                    let size = bytes.len();
                    assert_eq!(*datatype, c_s1_nullterm(size), "swmr={swmr} @{name}");
                }
            }
            std::fs::remove_file(&path).ok();
        }
    }

    /// C `openFile` starts SWMR whatever the write mode (NDFileHDF5.cpp:323).
    #[test]
    fn swmr_mode_opens_swmr_in_every_write_mode() {
        for mode in [NDFileMode::Single, NDFileMode::Capture, NDFileMode::Stream] {
            let path = temp_path("hdf5_swmr_every_mode");
            let mut writer = Hdf5Writer::new();
            writer.set_swmr_mode(true);
            writer.set_num_capture(0);
            let arr = Arc::new(ramp_u16(4, 5));
            writer.open_file(&path, opened(mode), &arr).unwrap();
            assert!(writer.is_swmr_active(), "{mode:?}");
            writer.write_file(&arr).unwrap();
            writer.close_file().unwrap();
            let h5 = H5File::open(&path).unwrap();
            let ds = h5.dataset("entry/instrument/detector/data").unwrap();
            let want = if mode == NDFileMode::Single {
                vec![5, 4]
            } else {
                vec![1, 5, 4]
            };
            assert_eq!(ds.shape(), want, "{mode:?}");
            assert_eq!(ds.read_raw::<u16>().unwrap(), u16_pixels(&arr), "{mode:?}");
            std::fs::remove_file(&path).ok();
        }
    }

    /// C `openFile` runs `configureCompression` for a SWMR file as well
    /// (NDFileHDF5.cpp:279, :3890), so the frame's codec reaches the params.
    #[test]
    fn a_swmr_open_takes_the_frame_codec_override() {
        let path = temp_path("hdf5_swmr_override");
        let mut writer = Hdf5Writer::new();
        writer.set_swmr_mode(true);
        let zlib = crate::codec::compress_zlib(&ramp_u16(4, 5), 4).unwrap();
        writer
            .open_file(
                &path,
                NDFileOpenMode::WRITE | NDFileOpenMode::MULTIPLE,
                &zlib,
            )
            .unwrap();
        assert_eq!(
            writer.open_posts.lock().posts,
            [
                OpenPost::SwmrCbCounter(0),
                OpenPost::Layout(true, "Default layout selected"),
                OpenPost::Compression(CompressionParam::Type, 3),
                OpenPost::Compression(CompressionParam::ZLevel, 4)
            ]
        );
        assert_eq!(
            writer
                .settings()
                .get(|s| (s.compression_type, s.z_compress_level)),
            (3, 4)
        );
        writer.close_file().unwrap();
        std::fs::remove_file(&path).ok();
    }

    /// C posts the nbit datatype HDF5 reads back (NDFileHDF5.cpp:3362-3371):
    /// H5Tset_precision refuses 0 and a float narrower than its type
    /// (H5Tprecis.c), leaving the native precision; the offset always holds.
    #[test]
    fn an_nbit_scheme_posts_the_hdf5_readback() {
        let cases = [
            (NDDataType::UInt16, 10, 3, (3, 10)),
            (NDDataType::UInt16, 0, 0, (0, 16)),
            (NDDataType::Float32, 10, 0, (0, 32)),
        ];
        for (dt, precision, offset, want) in cases {
            let path = temp_path("hdf5_nbit_readback");
            let mut writer = Hdf5Writer::new();
            writer.set_compression_type(COMPRESS_NBIT);
            writer.set_nbit_precision(precision);
            writer.set_nbit_offset(offset);
            let arr = NDArray::new(vec![NDDimension::new(4), NDDimension::new(4)], dt);
            writer
                .open_file(&path, NDFileOpenMode::WRITE, &arr)
                .unwrap();
            writer.write_file(&Arc::new(arr)).unwrap();
            writer.close_file().unwrap();
            assert_eq!(
                writer.open_posts.lock().posts,
                [
                    OpenPost::SwmrCbCounter(0),
                    OpenPost::Layout(true, "Default layout selected"),
                    OpenPost::Compression(CompressionParam::NbitOffset, want.0),
                    OpenPost::Compression(CompressionParam::NbitPrecision, want.1)
                ],
                "{dt:?} precision {precision} offset {offset}"
            );
            std::fs::remove_file(&path).ok();
        }
    }

    /// C stores a writer setting inside the write, so a Capture written
    /// right after it opens the file with it.
    #[test]
    fn a_writer_setting_reaches_the_writer_inside_the_write() {
        let (proc, mut base) = hdf5_port();
        let compression = idx(&base, "HDF5_compressionType");
        let extra = idx(&base, "HDF5_nExtraDims");
        proc.validate_int32(
            compression,
            0,
            3,
            WriteVerdict::Accept(3),
            &mut ad_core_rs::plugin::runtime::WritePort::new(&mut base),
        );
        proc.validate_int32(
            extra,
            0,
            2,
            WriteVerdict::Accept(2),
            &mut ad_core_rs::plugin::runtime::WritePort::new(&mut base),
        );
        assert_eq!(proc.settings.get(|s| s.compression_type), 3);
        assert_eq!(proc.settings.get(|s| s.n_extra_dims), 2);

        // :1804-1813: an unbounded capture drops the grid unless positions
        // place the frames.
        let num_capture = idx(&base, "NUM_CAPTURE");
        base.set_int32_param(idx(&base, "HDF5_posRunning"), 0, 1)
            .unwrap();
        proc.validate_int32(
            num_capture,
            0,
            0,
            WriteVerdict::Accept(0),
            &mut ad_core_rs::plugin::runtime::WritePort::new(&mut base),
        );
        assert_eq!(proc.settings.get(|s| s.n_extra_dims), 2);
        base.set_int32_param(idx(&base, "HDF5_posRunning"), 0, 0)
            .unwrap();
        proc.validate_int32(
            num_capture,
            0,
            0,
            WriteVerdict::Accept(0),
            &mut ad_core_rs::plugin::runtime::WritePort::new(&mut base),
        );
        assert_eq!(proc.settings.get(|s| s.n_extra_dims), 0);
    }

    /// C stores an HDF5 setting in the param library and reads it at the next
    /// open, so a setting write while a file write holds the writer returns at
    /// once instead of waiting for the write.
    #[test]
    fn a_setting_write_during_a_busy_write_returns_at_once() {
        let (proc, mut base) = hdf5_port();
        let compression = idx(&base, "HDF5_compressionType");
        let writer = Arc::clone(&proc.ctrl.lock().writer);
        assert!(
            crate::test_util::returns_while_busy(&writer, || {
                proc.validate_int32(
                    compression,
                    0,
                    3,
                    WriteVerdict::Accept(3),
                    &mut ad_core_rs::plugin::runtime::WritePort::new(&mut base),
                );
            }),
            "the setting write waited for the file write"
        );
        let mut w = writer.lock();
        assert_eq!(w.cfg.compression_type, 0, "not read before the next open");
        w.read_settings();
        assert_eq!(w.cfg.compression_type, 3);
    }

    /// C loads an XMLFileName containing `<?xml` from memory
    /// (NDFileHDF5LayoutXML.cpp:129-130), a path from disk, and the default
    /// layout for an empty one (NDFileHDF5.cpp:2154-2163).
    #[test]
    fn a_layout_write_loads_inline_xml_a_file_or_the_default() {
        let (proc, base) = hdf5_port();
        let layout = idx(&base, "HDF5_layoutFilename");
        let valid = idx(&base, "HDF5_layoutValid");
        let write =
            |v: &str| proc.validate_octet(layout, 0, v, WriteVerdict::Accept(v.to_owned()), &base);
        let side_valid = |out: &WriteOutcome<String>| {
            out.side.iter().rev().find_map(|p| match p {
                ad_core_rs::plugin::runtime::SideParam::Int32 { reason, value, .. }
                    if *reason == valid =>
                {
                    Some(*value)
                }
                _ => None,
            })
        };
        let xml = r#"<?xml version="1.0"?><hdf5_layout><group name="inline"><dataset name="data" source="detector" det_default="true"/></group></hdf5_layout>"#;

        let out = write(xml);
        assert_eq!(out.verdict, WriteVerdict::Accept(xml.to_owned()));
        assert_eq!(side_valid(&out), Some(1));
        {
            let c = proc.ctrl.lock();
            c.writer.lock().load_layout().unwrap();
            assert_eq!(
                c.writer.lock().layout.as_ref().unwrap().groups[0].name,
                "inline"
            );
        }

        let file = temp_path("layout_file").with_extension("xml");
        std::fs::write(&file, xml.replace("inline", "fromfile")).unwrap();
        let out = write(file.to_str().unwrap());
        assert_eq!(side_valid(&out), Some(1));
        {
            let c = proc.ctrl.lock();
            c.writer.lock().load_layout().unwrap();
            assert_eq!(
                c.writer.lock().layout.as_ref().unwrap().groups[0].name,
                "fromfile"
            );
        }
        std::fs::remove_file(&file).ok();

        let out = write("");
        assert_eq!(side_valid(&out), Some(1));
        let c = proc.ctrl.lock();
        c.writer.lock().load_layout().unwrap();
        assert!(
            c.writer.lock().layout.is_some(),
            "the default layout is loaded"
        );
    }

    /// C re-reads XMLFileName at open and fails it (NDFileHDF5.cpp:213-217):
    /// a refused layout write still leaves the name, and the Stream capture
    /// that follows fails in `verifyLayoutXMLFile`, before the file exists.
    #[test]
    fn a_stream_capture_with_a_malformed_layout_fails_to_open() {
        let (proc, mut base) = hdf5_port();
        let dir = std::env::temp_dir().join(format!("hdf5-badlayout-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let bad = dir.join("bad.xml");
        std::fs::write(&bad, "<hdf5_layout><group").unwrap();
        let octet = |name: &str, v: &str| {
            proc.validate_octet(
                idx(&base, name),
                0,
                v,
                WriteVerdict::Accept(v.to_owned()),
                &base,
            )
        };
        octet("FILE_PATH", &format!("{}/", dir.display()));
        octet("FILE_NAME", "s");
        octet("FILE_TEMPLATE", "%s%s_%d.h5");
        let out = octet("HDF5_layoutFilename", bad.to_str().unwrap());
        assert!(matches!(out.verdict, WriteVerdict::Refuse { .. }));
        let mut int = |name: &str, v: i32| {
            proc.validate_int32(
                idx(&base, name),
                0,
                v,
                WriteVerdict::Accept(v),
                &mut ad_core_rs::plugin::runtime::WritePort::new(&mut base),
            )
        };
        int("WRITE_MODE", 2);
        proc.ctrl.lock().latest_array = Some(Arc::new(NDArray::new(
            vec![NDDimension::new(4), NDDimension::new(4)],
            NDDataType::UInt8,
        )));
        let out = int("CAPTURE", 1);
        assert_eq!(out.verdict, WriteVerdict::Accept(0));
        let status = idx(&base, "WRITE_STATUS");
        assert!(out.side.iter().any(|p| matches!(p,
            ad_core_rs::plugin::runtime::SideParam::Int32 { reason, value: 1, .. } if *reason == status)));
        assert!(!proc.ctrl.lock().file_base.is_open());
        assert_eq!(
            out.trace.first().map(|(_, l)| l.as_str()),
            Some(LayoutFault::ParserError.line())
        );
        assert!(!dir.join("s_0.h5").exists());
        std::fs::remove_dir_all(&dir).ok();
    }

    /// C flushTask clears SWMRFlushNow once it has flushed (:398).
    #[test]
    fn a_swmr_flush_clears_the_request_as_c_flush_task_does() {
        let (proc, base) = hdf5_port();
        let flush = idx(&base, "HDF5_SWMRFlushNow");
        let updates = proc
            .on_param_change(
                flush,
                &PluginParamSnapshot {
                    enable_callbacks: true,
                    reason: flush,
                    addr: 0,
                    value: ad_core_rs::plugin::runtime::ParamChangeValue::Int32(1),
                },
            )
            .param_updates;
        assert!(updates.iter().any(|u| matches!(
            u,
            ParamUpdate::Int32 { reason, value: 0, .. } if *reason == flush
        )));
    }

    #[test]
    fn a_bad_layout_file_is_kept_and_refused_as_c_write_octet_does() {
        let (proc, base) = hdf5_port();
        let xml = idx(&base, "HDF5_layoutFilename");
        let dir = tempfile::tempdir().unwrap();
        let junk = dir.path().join("junk.xml");
        std::fs::write(&junk, "<hdf5_layout><group").unwrap();
        let v = |value: &str| {
            proc.validate_octet(xml, 0, value, WriteVerdict::Accept(value.into()), &base)
                .verdict
        };
        let refused = |value: &str| WriteVerdict::Refuse {
            keep: value.to_owned(),
            message: format!("NDFileHDF5:writeOctet: status=3, function={xml}, value={value}"),
        };

        assert_eq!(v(""), WriteVerdict::Accept(String::new()));
        assert_eq!(v("/nonexistent.xml"), refused("/nonexistent.xml"));
        let junk = junk.to_str().unwrap();
        assert_eq!(v(junk), refused(junk));
        assert_eq!(
            v("<?xml version=\"1.0\"?><bad"),
            refused("<?xml version=\"1.0\"?><bad")
        );

        // verifyLayoutXMLFile (:2145-2174) sets both params inside the write.
        let side = |value: &str| {
            proc.validate_octet(xml, 0, value, WriteVerdict::Accept(value.into()), &base)
                .side
        };
        let set = |valid, msg: &str| {
            vec![
                ad_core_rs::plugin::runtime::SideParam::Int32 {
                    reason: idx(&base, "HDF5_layoutValid"),
                    addr: 0,
                    value: valid,
                },
                ad_core_rs::plugin::runtime::SideParam::Octet {
                    reason: idx(&base, "HDF5_layoutErrorMsg"),
                    addr: 0,
                    value: msg.to_owned(),
                },
            ]
        };
        assert_eq!(side(""), set(1, "Default layout selected"));
        assert_eq!(
            side("/nonexistent.xml"),
            set(0, "XML description file cannot be opened")
        );
        assert_eq!(side(junk), set(0, "XML description file parser error"));
        let good = dir.path().join("good.xml");
        std::fs::write(&good, DEFAULT_LAYOUT_XML).unwrap();
        assert_eq!(side(good.to_str().unwrap()), set(1, ""));
    }

    /// C verifyLayoutXMLFile asks libxml only whether the document is well
    /// formed (NDFileHDF5.cpp:2163-2174): `<foo/>` passes, and the open then
    /// fails in createFileLayout, which finds no `<hdf5_layout>` (:279-284).
    #[test]
    fn a_well_formed_non_layout_passes_verify_and_fails_the_open() {
        assert_eq!(verify_layout("<?xml version=\"1.0\"?><foo/>"), Ok(""));
        let dir = tempfile::tempdir().unwrap();
        let foo = dir.path().join("foo.xml");
        std::fs::write(&foo, "<foo/>").unwrap();
        assert_eq!(verify_layout(foo.to_str().unwrap()), Ok(""));

        let mut writer = Hdf5Writer::new();
        writer.set_layout_filename(foo.to_str().unwrap());
        let arr = NDArray::new(vec![NDDimension::new(4)], NDDataType::UInt8);
        let err = writer
            .open_file(&dir.path().join("f.h5"), NDFileOpenMode::WRITE, &arr)
            .unwrap_err();
        assert_eq!(
            err.lines(),
            ["NDFileHDF5::openFile ERROR Failed to create the specified file layout\n"]
        );
    }

    /// XMLValid and LayoutErrorMsg for each layout file, as C's libxml2
    /// 2.9.4 `verifyLayoutXMLFile` set them.
    #[test]
    fn layout_xml_validity_matches_c_libxml() {
        let corpus: &[(&str, &[u8], bool)] = &[
            ("01_foo", b"<foo/>", true),
            ("02_unterm", b"<hdf5_layout><group", false),
            ("03_empty", b"", false),
            ("04_ws", b"   \n", false),
            ("05_mismatch", b"<a></b>", false),
            ("06_cross", b"<a><b></a></b>", false),
            ("07_tworoots", b"<a/><b/>", false),
            ("08_textbefore", b"text<a/>", false),
            ("09_textafter", b"<a/>text", false),
            ("10_text", b"<a>text</a>", true),
            ("11_decl", b"<?xml version=\"1.0\"?><a/>", true),
            ("12_comments", b"<!-- c --><a/><!-- d -->", true),
            ("13_dupattr", b"<a x=\"1\" x=\"2\"/>", false),
            ("14_unquoted", b"<a x=1/>", false),
            ("15_lt_in_attr", b"<a x=\"<\"/>", false),
            ("16_amp_attr", b"<a x=\"&amp;\"/>", true),
            ("17_undef_entity", b"<a>&foo;</a>", false),
            ("18_charref", b"<a>&#65;</a>", true),
            ("19_lt_ent", b"<a>&lt;</a>", true),
            ("20_cdata", b"<a><![CDATA[<x>]]></a>", true),
            ("21_unclosed", b"<a>", false),
            ("22_close_only", b"</a>", false),
            ("23_badname", b"<1a/>", false),
            ("24_attr_novalue", b"<a b/>", false),
            ("25_bare_amp", b"<a>x & y</a>", false),
            ("26_gt_text", b"<a>x > y</a>", true),
            ("27_decl_only", b"<?xml version=\"1.0\"?>", false),
            ("28_decl_late", b" <?xml version=\"1.0\"?><a/>", false),
            ("29_dashdash", b"<a><!-- -- --></a>", false),
            ("30_doctype", b"<!DOCTYPE a><a/>", true),
            ("31_ns", b"<a xmlns:p=\"u\"><p:b/></a>", true),
            ("32_unbound", b"<p:b/>", true),
            ("33_squote", b"<a x='1'/>", true),
            ("34_cdata_end", b"<a>]]></a>", false),
            ("35_bom", b"\xef\xbb\xbf<a/>", true),
            ("36_nl_in_tag", b"<a\n/>", true),
            ("37_noattrws", b"<a x=\"1\"y=\"2\"/>", false),
            ("38_space_before_name", b"< a/>", false),
            ("39_slash_space", b"<a/ >", false),
            ("40_pi", b"<?pi data?><a/>", true),
            ("41_xml_pi_inside", b"<a><?xml version=\"1.0\"?></a>", false),
            ("42_internal_subset", b"<!DOCTYPE a [<!ENTITY e \"x\">]><a>&e;</a>", true),
            ("43_close_space", b"<a></a >", true),
            ("44_hexref", b"<a>&#x41;</a>", true),
            ("45_badref", b"<a>&#xZZ;</a>", false),
            ("46_attr_amp_bare", b"<a x=\"a&b\"/>", false),
            ("47_layout", b"<?xml version=\"1.0\"?><hdf5_layout><group name=\"e\"><dataset name=\"d\" source=\"detector\" det_default=\"true\"/></group></hdf5_layout>", true),
            ("48_group_noname", b"<hdf5_layout><group><dataset name=\"d\" source=\"detector\"/></group></hdf5_layout>", true),
            ("49_unknown_elem", b"<hdf5_layout><group name=\"e\"><bogus/></group></hdf5_layout>", true),
            ("50_comment_unterm", b"<a><!-- x</a>", false),
            ("51_text_only", b"hello", false),
            ("52_nul_ref", b"<a>&#0;</a>", false),
            ("53_attr_tab_ws", b"<a\tx=\"1\"\t/>", true),
            ("54_quote_mix", b"<a x=\"1'/>", false),
            ("55_dup_ns_attr", b"<a xmlns:p=\"u\" xmlns:q=\"u\" p:x=\"1\" q:x=\"2\"/>", true),
            ("56_decl_bad", b"<?xml version=\"2.0\"?><a/>", false),
            ("57_decl_enc", b"<?xml version=\"1.0\" encoding=\"UTF-8\"?><a/>", true),
            ("58_ws_after_root", b"<a/>\n\n", true),
            ("59_utf8", b"<a>\xc3\xa9</a>", true),
            ("60_latin1_bytes", b"<a>\xe9</a>", false),
        ];
        let dir = tempfile::tempdir().unwrap();
        for &(name, doc, valid) in corpus {
            let path = dir.path().join(name);
            std::fs::write(&path, doc).unwrap();
            let want = if valid {
                Ok("")
            } else {
                Err(LayoutFault::ParserError)
            };
            assert_eq!(verify_layout(path.to_str().unwrap()), want, "{name}");
        }
    }

    #[test]
    fn extra_dims_set_num_capture_as_c_calc_num_frames_does() {
        let (proc, mut base) = hdf5_port();
        let (num_capture, extra, size_n, size_x, pos) = (
            idx(&base, "NUM_CAPTURE"),
            idx(&base, "HDF5_nExtraDims"),
            idx(&base, "HDF5_extraDimSizeN"),
            idx(&base, "HDF5_extraDimSizeX"),
            idx(&base, "HDF5_posRunning"),
        );
        let write = |base: &mut asyn_rs::port::PortDriverBase, reason, value| {
            proc.validate_int32(
                reason,
                0,
                value,
                WriteVerdict::Accept(value),
                &mut ad_core_rs::plugin::runtime::WritePort::new(base),
            )
        };
        let side = |out: WriteOutcome<i32>, target| {
            out.side.iter().find_map(|p| match p {
                ad_core_rs::plugin::runtime::SideParam::Int32 { reason, value, .. }
                    if *reason == target =>
                {
                    Some(*value)
                }
                _ => None,
            })
        };
        base.set_int32_param(size_n, 0, 3).unwrap();
        base.set_int32_param(size_x, 0, 4).unwrap();
        // :1797 "if (value > 0) this->calcNumFrames();"
        assert_eq!(side(write(&mut base, extra, 1), num_capture), Some(12));
        // calcNumFrames :2448 "if (numExtraDims == 0) return;"
        assert_eq!(side(write(&mut base, size_x, 5), num_capture), None);
        base.set_int32_param(extra, 0, 1).unwrap();
        // :1888 "this->calcNumFrames();" reads the size just written.
        assert_eq!(side(write(&mut base, size_x, 5), num_capture), Some(15));

        // :1822-1825: NumCapture above the grid keeps the grid; below it keeps the write.
        assert_eq!(
            write(&mut base, num_capture, 20).verdict,
            WriteVerdict::Accept(12)
        );
        assert_eq!(
            write(&mut base, num_capture, 5).verdict,
            WriteVerdict::Accept(5)
        );

        // :1814 and :1834: an unbounded capture drops the grid
        // unless positions place the frames.
        base.set_int32_param(pos, 0, 1).unwrap();
        assert_eq!(side(write(&mut base, num_capture, 0), extra), None);
        base.set_int32_param(num_capture, 0, 0).unwrap();
        assert_eq!(side(write(&mut base, pos, 0), extra), Some(0));
        base.set_int32_param(pos, 0, 0).unwrap();
        assert_eq!(side(write(&mut base, num_capture, 0), extra), Some(0));
    }

    #[test]
    fn test_parse_fsync_on_close_env() {
        // Unset keeps the durable default.
        assert!(Hdf5Writer::parse_fsync_on_close_env(None));
        // The falsey set opts into the no-fsync fast close.
        for v in ["0", "false", "no", "off", "FALSE", "Off", "  no  "] {
            assert!(
                !Hdf5Writer::parse_fsync_on_close_env(Some(v)),
                "{v:?} should disable fsync-on-close"
            );
        }
        // Anything else — including empty and truthy values — stays durable.
        for v in ["", "1", "true", "yes", "on", "durable"] {
            assert!(
                Hdf5Writer::parse_fsync_on_close_env(Some(v)),
                "{v:?} should keep fsync-on-close"
            );
        }
    }

    #[test]
    fn test_no_fsync_close_writes_valid_file() {
        // The no-fsync fast close (`H5File::close_no_sync`) must still finalize
        // a complete, readable file — it only skips the durability fsync, not
        // the header/superblock writes.
        let path = temp_path("hdf5_no_fsync");
        let mut writer = Hdf5Writer::new();
        writer.fsync_on_close = false;

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

        let h5 = H5File::open(&path).unwrap();
        let ds = h5.dataset("entry/instrument/detector/data").unwrap();
        assert_eq!(ds.shape(), vec![4, 4]);
        let data: Vec<u8> = ds.read_raw().unwrap();
        assert_eq!(data[0], 0);
        assert_eq!(data[15], 15);
        drop(h5);

        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn test_write_multiple_frames() {
        let path = temp_path("hdf5_multi");
        let mut writer = Hdf5Writer::new();
        writer.set_num_capture(0);

        let mut arr = NDArray::new(
            vec![NDDimension::new(4), NDDimension::new(4)],
            NDDataType::UInt8,
        );
        // Mark each frame distinctly so we can verify per-frame placement.
        for f in 0..3u8 {
            if let NDDataViewMut::U8(v) = arr.data_mut().view_mut() {
                for x in v.iter_mut() {
                    *x = f;
                }
            }
            if f == 0 {
                writer
                    .open_file(
                        &path,
                        NDFileOpenMode::WRITE | NDFileOpenMode::MULTIPLE,
                        &arr,
                    )
                    .unwrap();
            }
            writer.write_file(&Arc::new(arr.clone())).unwrap();
        }
        writer.close_file().unwrap();

        assert!(writer.supports_multiple_arrays());
        assert_eq!(writer.frame_count(), 3);

        let data = std::fs::read(&path).unwrap();
        assert_eq!(&data[0..8], b"\x89HDF\r\n\x1a\n");

        // Single extensible dataset [3, 4, 4] — NOT one dataset per frame.
        let h5 = H5File::open(&path).unwrap();
        let names = h5.dataset_names();
        assert!(names.contains(&"entry/instrument/detector/data".to_string()));
        assert!(
            !names.contains(&"data_1".to_string()),
            "must not write per-frame datasets"
        );
        let ds = h5.dataset("entry/instrument/detector/data").unwrap();
        assert_eq!(
            ds.shape(),
            vec![3, 4, 4],
            "rank/shape must be [nframes,Y,X]"
        );
        let raw: Vec<u8> = ds.read_raw().unwrap();
        assert_eq!(raw.len(), 3 * 4 * 4);
        // Frame 0 all zeros, frame 1 all ones, frame 2 all twos.
        assert_eq!(raw[0], 0);
        assert_eq!(raw[16], 1);
        assert_eq!(raw[32], 2);

        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn test_sub_frame_chunking() {
        // nRowChunks/nColChunks that divide the frame produce a sub-frame
        // chunk grid written via write_chunk_at tiles; the dataset shape
        // stays exactly [N, Y, X] (no padding) and the data round-trips.
        let path = temp_path("hdf5_subchunk");
        let mut writer = Hdf5Writer::new();
        writer.set_num_capture(0);
        writer.set_chunk_size_auto(false); // honor explicit chunk sizes
        writer.set_n_row_chunks(4); // Y = 8 → 2 row tiles
        writer.set_n_col_chunks(4); // X = 8 → 2 col tiles

        let mut arr = NDArray::new(
            vec![NDDimension::new(8), NDDimension::new(8)],
            NDDataType::UInt16,
        );
        for f in 0..3u16 {
            if let NDDataViewMut::U16(v) = arr.data_mut().view_mut() {
                for (i, x) in v.iter_mut().enumerate() {
                    *x = f * 1000 + i as u16;
                }
            }
            if f == 0 {
                writer
                    .open_file(
                        &path,
                        NDFileOpenMode::WRITE | NDFileOpenMode::MULTIPLE,
                        &arr,
                    )
                    .unwrap();
            }
            writer.write_file(&Arc::new(arr.clone())).unwrap();
        }
        writer.close_file().unwrap();

        let h5 = H5File::open(&path).unwrap();
        let ds = h5.dataset("entry/instrument/detector/data").unwrap();
        assert_eq!(ds.shape(), vec![3, 8, 8], "shape must not be chunk-padded");
        assert_eq!(
            ds.chunk_dims(),
            Some(vec![1, 4, 4]),
            "chunk grid must be the sub-frame tile size"
        );
        let raw: Vec<u16> = ds.read_raw().unwrap();
        assert_eq!(raw.len(), 3 * 64);
        for f in 0..3u16 {
            for i in 0..64usize {
                assert_eq!(
                    raw[f as usize * 64 + i],
                    f * 1000 + i as u16,
                    "frame {} elem {}",
                    f,
                    i
                );
            }
        }

        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn test_sub_frame_chunking_with_compression() {
        // Sub-frame chunk tiles must round-trip through a filter pipeline:
        // each write_chunk_at tile is compressed independently.
        let path = temp_path("hdf5_subchunk_zlib");
        let mut writer = Hdf5Writer::new();
        writer.set_num_capture(0);
        writer.set_chunk_size_auto(false);
        writer.set_n_row_chunks(4);
        writer.set_n_col_chunks(4);
        writer.set_compression_type(COMPRESS_ZLIB);

        let mut arr = NDArray::new(
            vec![NDDimension::new(8), NDDimension::new(8)],
            NDDataType::UInt16,
        );
        for f in 0..2u16 {
            if let NDDataViewMut::U16(v) = arr.data_mut().view_mut() {
                for (i, x) in v.iter_mut().enumerate() {
                    *x = f * 100 + i as u16;
                }
            }
            if f == 0 {
                writer
                    .open_file(
                        &path,
                        NDFileOpenMode::WRITE | NDFileOpenMode::MULTIPLE,
                        &arr,
                    )
                    .unwrap();
            }
            writer.write_file(&Arc::new(arr.clone())).unwrap();
        }
        writer.close_file().unwrap();

        let h5 = H5File::open(&path).unwrap();
        let ds = h5.dataset("entry/instrument/detector/data").unwrap();
        assert_eq!(ds.shape(), vec![2, 8, 8]);
        assert_eq!(ds.chunk_dims(), Some(vec![1, 4, 4]));
        let raw: Vec<u16> = ds.read_raw().unwrap();
        for f in 0..2u16 {
            for i in 0..64usize {
                assert_eq!(raw[f as usize * 64 + i], f * 100 + i as u16);
            }
        }

        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn test_non_dividing_chunk_is_honored_and_extent_trimmed() {
        // A chunk size that does not divide the frame is honored as-is;
        // write_chunk_at rounds the extent up, and close_file's set_extent
        // trims the dataset shape back to the exact [N, Y, X].
        let path = temp_path("hdf5_subchunk_nd");
        let mut writer = Hdf5Writer::new();
        writer.set_num_capture(0);
        writer.set_chunk_size_auto(false); // honor explicit chunk sizes
        writer.set_n_row_chunks(3); // Y = 8, 8 % 3 != 0 → honored
        writer.set_n_col_chunks(4); // X = 8 → honored

        let mut arr = NDArray::new(
            vec![NDDimension::new(8), NDDimension::new(8)],
            NDDataType::UInt16,
        );
        if let NDDataViewMut::U16(v) = arr.data_mut().view_mut() {
            for (i, x) in v.iter_mut().enumerate() {
                *x = i as u16;
            }
        }
        writer
            .open_file(
                &path,
                NDFileOpenMode::WRITE | NDFileOpenMode::MULTIPLE,
                &arr,
            )
            .unwrap();
        writer.write_file(&Arc::new(arr.clone())).unwrap();
        writer.write_file(&Arc::new(arr)).unwrap();
        writer.close_file().unwrap();

        let h5 = H5File::open(&path).unwrap();
        let ds = h5.dataset("entry/instrument/detector/data").unwrap();
        assert_eq!(ds.shape(), vec![2, 8, 8], "extent trimmed, not padded");
        assert_eq!(ds.chunk_dims(), Some(vec![1, 3, 4]));
        let raw: Vec<u16> = ds.read_raw().unwrap();
        assert_eq!(raw.len(), 2 * 64);
        for i in 0..64usize {
            assert_eq!(raw[i], i as u16);
            assert_eq!(raw[64 + i], i as u16);
        }

        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn test_n_frames_chunks_band() {
        // HDF5_nFramesChunks groups frames into a multi-frame chunk band; the
        // logical frame count stays exact even when the last band is partial.
        let path = temp_path("hdf5_framechunks");
        let mut writer = Hdf5Writer::new();
        writer.set_num_capture(0);
        writer.set_chunk_size_auto(false);
        writer.set_n_frames_chunks(2); // 2 frames per chunk band

        let mut arr = NDArray::new(
            vec![NDDimension::new(4), NDDimension::new(4)],
            NDDataType::UInt16,
        );
        // 5 frames → bands [0,1], [2,3], [4] (partial).
        for f in 0..5u16 {
            if let NDDataViewMut::U16(v) = arr.data_mut().view_mut() {
                for (i, x) in v.iter_mut().enumerate() {
                    *x = f * 1000 + i as u16;
                }
            }
            if f == 0 {
                writer
                    .open_file(
                        &path,
                        NDFileOpenMode::WRITE | NDFileOpenMode::MULTIPLE,
                        &arr,
                    )
                    .unwrap();
            }
            writer.write_file(&Arc::new(arr.clone())).unwrap();
        }
        writer.close_file().unwrap();

        let h5 = H5File::open(&path).unwrap();
        let ds = h5.dataset("entry/instrument/detector/data").unwrap();
        assert_eq!(ds.shape(), vec![5, 4, 4], "exact frame count, no padding");
        assert_eq!(ds.chunk_dims(), Some(vec![2, 4, 4]));
        let raw: Vec<u16> = ds.read_raw().unwrap();
        for f in 0..5u16 {
            for i in 0..16usize {
                assert_eq!(raw[f as usize * 16 + i], f * 1000 + i as u16);
            }
        }

        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn test_frames_chunks_with_sub_frame_tiles() {
        // Full chunk geometry: nFramesChunks AND sub-frame row/col tiling at
        // once — exercises the complete flush_band [fc, rc, cc] tile grid
        // with a partial final band.
        let path = temp_path("hdf5_full_chunk");
        let mut writer = Hdf5Writer::new();
        writer.set_num_capture(0);
        writer.set_chunk_size_auto(false);
        writer.set_n_frames_chunks(2); // 2 frames per band
        writer.set_n_row_chunks(4); // Y = 8 → 2 row tiles
        writer.set_n_col_chunks(4); // X = 8 → 2 col tiles

        let mut arr = NDArray::new(
            vec![NDDimension::new(8), NDDimension::new(8)],
            NDDataType::UInt16,
        );
        // 3 frames → band [0,1] full, band [2] partial; 2x2 tiles each.
        for f in 0..3u16 {
            if let NDDataViewMut::U16(v) = arr.data_mut().view_mut() {
                for (i, x) in v.iter_mut().enumerate() {
                    *x = f * 1000 + i as u16;
                }
            }
            if f == 0 {
                writer
                    .open_file(
                        &path,
                        NDFileOpenMode::WRITE | NDFileOpenMode::MULTIPLE,
                        &arr,
                    )
                    .unwrap();
            }
            writer.write_file(&Arc::new(arr.clone())).unwrap();
        }
        writer.close_file().unwrap();

        let h5 = H5File::open(&path).unwrap();
        let ds = h5.dataset("entry/instrument/detector/data").unwrap();
        assert_eq!(ds.shape(), vec![3, 8, 8], "exact frame count");
        assert_eq!(ds.chunk_dims(), Some(vec![2, 4, 4]));
        let raw: Vec<u16> = ds.read_raw().unwrap();
        assert_eq!(raw.len(), 3 * 64);
        for f in 0..3u16 {
            for i in 0..64usize {
                assert_eq!(
                    raw[f as usize * 64 + i],
                    f * 1000 + i as u16,
                    "frame {} elem {}",
                    f,
                    i
                );
            }
        }

        std::fs::remove_file(&path).ok();
    }

    /// C `openFile`/`writeFile` fill `pFileAttributes` from `getAttributes`
    /// with the frame's list copied on top (NDFileHDF5.cpp:250-252,
    /// :1421-1438), so the plugin's own set gets attribute datasets.
    #[test]
    fn the_plugin_attributes_get_datasets_under_the_frame() {
        use crate::test_util::{auto_save_to, publish, wait_saved, with_attributes_file};
        let xml = r#"<Attributes>
            <Attribute name="plugin_only" type="PARAM" source="AUTO_SAVE" datatype="INT"/>
            <Attribute name="count" type="CONST" source="99"/>
        </Attributes>"#;
        let (handle, _rx, _jh) = with_attributes_file("HDF5_ATTRS", Hdf5FileProcessor::new(), xml);
        let path = temp_path("plugin_attrs");
        auto_save_to(&handle, &path);
        let mut arr = NDArray::new(vec![NDDimension::new(4)], NDDataType::UInt8);
        arr.attributes.add(NDAttribute::new_static(
            "count",
            "",
            NDAttrSource::Driver,
            NDAttrValue::Int32(10),
        ));
        publish(&handle, &arr);
        wait_saved(&handle, 1);
        let h5 = H5File::open(&path).unwrap();
        let values = |name: &str| -> Vec<i32> {
            h5.dataset(&format!("entry/instrument/NDAttributes/{name}"))
                .unwrap()
                .read_raw()
                .unwrap()
        };
        assert_eq!(values("plugin_only"), vec![1]);
        assert_eq!(values("count"), vec![10]);
        std::fs::remove_file(&path).ok();
    }

    /// C `addDefaultAttributes` puts the array's id and timestamps into
    /// `pFileAttributes` (NDFileHDF5.cpp:3663-3677), so every file carries
    /// them as attribute datasets.
    #[test]
    fn the_default_attributes_get_datasets() {
        let path = temp_path("hdf5_default_attrs");
        let mut writer = Hdf5Writer::new();
        writer.set_num_capture(0);
        let mk = |id: i32, sec: u32| {
            let mut arr = NDArray::new(vec![NDDimension::new(4)], NDDataType::UInt8);
            arr.unique_id = id;
            arr.time_stamp = f64::from(sec) + 0.5;
            arr.timestamp.sec = sec;
            arr.timestamp.nsec = 7;
            arr
        };
        let a0 = mk(3, 100);
        writer
            .open_file(&path, NDFileOpenMode::WRITE | NDFileOpenMode::MULTIPLE, &a0)
            .unwrap();
        writer.write_file(&Arc::new(a0)).unwrap();
        writer.write_file(&Arc::new(mk(4, 101))).unwrap();
        writer.close_file().unwrap();

        let h5 = H5File::open(&path).unwrap();
        let ds = |name: &str| {
            h5.dataset(&format!("entry/instrument/NDAttributes/{name}"))
                .unwrap()
        };
        assert_eq!(ds("NDArrayUniqueId").read_raw::<i32>().unwrap(), vec![3, 4]);
        assert_eq!(
            ds("NDArrayTimeStamp").read_raw::<f64>().unwrap(),
            vec![100.5, 101.5]
        );
        assert_eq!(
            ds("NDArrayEpicsTSSec").read_raw::<u32>().unwrap(),
            vec![100, 101]
        );
        assert_eq!(
            ds("NDArrayEpicsTSnSec").read_raw::<u32>().unwrap(),
            vec![7, 7]
        );
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn test_attribute_datasets() {
        assert_attribute_datasets(false);
    }

    /// createAttributeDataset runs before startSWMR (NDFileHDF5.cpp:297-326),
    /// so a SWMR file carries the same datasets; each test below runs both.
    #[test]
    fn swmr_attribute_datasets() {
        assert_attribute_datasets(true);
    }

    fn assert_attribute_datasets(swmr: bool) {
        let path = temp_path("hdf5_attr_ds");
        let mut writer = Hdf5Writer::new();
        writer.set_swmr_mode(swmr);
        writer.set_num_capture(0);

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
        writer
            .open_file(&path, NDFileOpenMode::WRITE | NDFileOpenMode::MULTIPLE, &a0)
            .unwrap();
        writer.write_file(&Arc::new(a0)).unwrap();
        writer.write_file(&Arc::new(mk(0.75, 20))).unwrap();
        writer.write_file(&Arc::new(mk(1.25, 30))).unwrap();
        writer.close_file().unwrap();

        let h5 = H5File::open(&path).unwrap();
        // One HDF5 dataset per NDAttribute, under NDAttributes/, [nframes].
        let exp = h5
            .dataset("entry/instrument/NDAttributes/exposure")
            .unwrap();
        assert_eq!(exp.shape(), vec![3]);
        let exp_vals: Vec<f64> = exp.read_raw().unwrap();
        assert_eq!(exp_vals, vec![0.5, 0.75, 1.25]);
        assert_eq!(
            exp.attr("NDAttrSourceType").unwrap().read_string().unwrap(),
            "NDAttrSourceDriver"
        );

        let cnt = h5.dataset("entry/instrument/NDAttributes/count").unwrap();
        assert_eq!(cnt.shape(), vec![3]);
        // Numeric type preserved: i32, not stringified.
        let cnt_vals: Vec<i32> = cnt.read_raw().unwrap();
        assert_eq!(cnt_vals, vec![10, 20, 30]);

        std::fs::remove_file(&path).ok();
    }

    /// C writes an NDAttribute dataset only at its layout `when`
    /// (NDFileHDF5AttributeDataset.cpp:143): `OnFileOpen` once from the
    /// open-time list (NDFileHDF5.cpp:299), `OnFileClose` once from the last
    /// frame's (:1662), the default every frame.
    fn assert_attribute_datasets_follow_layout_when(swmr: bool) {
        let layout = std::env::temp_dir().join(format!(
            "adcore_layout_attr_ds_when_{}_{swmr}.xml",
            std::process::id()
        ));
        std::fs::write(
            &layout,
            r#"<hdf5_layout>
              <group name="entry">
                <dataset name="data" source="detector" det_default="true"/>
                <dataset name="at_open" source="ndattribute" ndattribute="A" when="OnFileOpen"/>
                <dataset name="at_close" source="ndattribute" ndattribute="B" when="OnFileClose"/>
                <dataset name="per_frame" source="ndattribute" ndattribute="C"/>
                <group name="NDAttributes" ndattr_default="true"/>
              </group>
            </hdf5_layout>"#,
        )
        .unwrap();
        let path = temp_path("hdf5_attr_ds_when");
        let mut writer = Hdf5Writer::new();
        writer.set_swmr_mode(swmr);
        writer.set_num_capture(0);
        writer.set_layout_filename(layout.to_str().unwrap());
        let mk = |v: i32| {
            let mut arr = NDArray::new(vec![NDDimension::new(4)], NDDataType::UInt8);
            for name in ["A", "B", "C"] {
                arr.attributes.add(NDAttribute::new_static(
                    name,
                    "",
                    NDAttrSource::Driver,
                    NDAttrValue::Int32(v),
                ));
            }
            arr
        };
        writer
            .open_file(
                &path,
                NDFileOpenMode::WRITE | NDFileOpenMode::MULTIPLE,
                &mk(1),
            )
            .unwrap();
        writer.write_file(&Arc::new(mk(2))).unwrap();
        writer.write_file(&Arc::new(mk(3))).unwrap();
        writer.close_file().unwrap();
        std::fs::remove_file(&layout).ok();

        let h5 = H5File::open(&path).unwrap();
        let read = |name: &str| -> Vec<i32> {
            h5.dataset(&format!("entry/{name}"))
                .unwrap()
                .read_raw()
                .unwrap()
        };
        assert_eq!(read("at_open"), [1], "swmr={swmr}");
        assert_eq!(read("at_close"), [3], "swmr={swmr}");
        assert_eq!(read("per_frame"), [2, 3], "swmr={swmr}");
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn attribute_datasets_follow_layout_when() {
        assert_attribute_datasets_follow_layout_when(false);
    }

    #[test]
    fn swmr_attribute_datasets_follow_layout_when() {
        assert_attribute_datasets_follow_layout_when(true);
    }

    /// Write two frames carrying `exposure` through `layout` with StorePerform
    /// on; returns the file.
    fn write_through_layout(swmr: bool, tag: &str, layout: &str) -> PathBuf {
        write_frames_through_layout(swmr, tag, layout, 2)
    }

    fn write_frames_through_layout(swmr: bool, tag: &str, layout: &str, frames: usize) -> PathBuf {
        let xml = std::env::temp_dir().join(format!(
            "adcore_layout_{tag}_{}_{swmr}.xml",
            std::process::id()
        ));
        std::fs::write(&xml, layout).unwrap();
        let path = temp_path(&format!("hdf5_{tag}_{swmr}"));
        let mut writer = Hdf5Writer::new();
        writer.set_swmr_mode(swmr);
        writer.set_num_capture(0);
        writer.set_store_performance(true);
        writer.set_layout_filename(xml.to_str().unwrap());
        let mk = || {
            let mut arr = NDArray::new(vec![NDDimension::new(4)], NDDataType::UInt8);
            arr.attributes.add(NDAttribute::new_static(
                "exposure",
                "",
                NDAttrSource::Driver,
                NDAttrValue::Float64(0.5),
            ));
            arr
        };
        writer
            .open_file(
                &path,
                NDFileOpenMode::WRITE | NDFileOpenMode::MULTIPLE,
                &mk(),
            )
            .unwrap();
        for _ in 0..frames {
            writer.write_file(&Arc::new(mk())).unwrap();
        }
        writer.close_file().unwrap();
        std::fs::remove_file(&xml).ok();
        path
    }

    /// An attribute with no known source is notset, whose `int8` type C
    /// `writeHdfAttributes` cannot write: it prints a line wherever it would
    /// write one, `createTree` depth first, then `createAttributeDataset`
    /// (NDFileHDF5.cpp:638-708, :789-792, :2816).
    #[test]
    fn a_notset_attribute_prints_c_unknown_type_line_where_c_writes_it() {
        let dir = tempfile::tempdir().unwrap();
        let xml = dir.path().join("notset.xml");
        std::fs::write(
            &xml,
            r#"<hdf5_layout><group name="entry">
              <attribute name="nosrc" value="x"/>
              <attribute name="bogus" source="bogus" value="y"/>
              <attribute name="kind" source="constant" value="7" type="int"/>
              <dataset name="data" source="detector" det_default="true">
                <attribute name="dnosrc"/></dataset>
              <dataset name="cnos" value="3" type="int"><attribute name="never"/></dataset>
              <dataset name="exp" source="ndattribute" ndattribute="exposure">
                <attribute name="xnosrc"/></dataset>
              <group name="b"><attribute name="bnull"/></group>
              <group name="a"><attribute name="anull"/>
                <group name="deep"><attribute name="dnull"/></group></group>
              <group name="NDAttributes" ndattr_default="true"/>
            </group></hdf5_layout>"#,
        )
        .unwrap();
        for swmr in [false, true] {
            for store in [true, false] {
                let path = dir.path().join(format!("notset_{swmr}_{store}.h5"));
                let mut writer = Hdf5Writer::new();
                writer.set_swmr_mode(swmr);
                writer.set_store_attributes(store);
                writer.set_layout_filename(xml.to_str().unwrap());
                let mut arr = NDArray::new(vec![NDDimension::new(4)], NDDataType::UInt8);
                arr.attributes.add(NDAttribute::new_static(
                    "exposure",
                    "",
                    NDAttrSource::Driver,
                    NDAttrValue::Float64(0.5),
                ));
                writer
                    .open_file(&path, NDFileOpenMode::WRITE, &arr)
                    .unwrap();
                let expected: &[&str] = if store {
                    &[
                        "dnosrc", "bogus", "nosrc", "anull", "dnull", "bnull", "xnosrc",
                    ]
                } else {
                    &["dnosrc"]
                };
                let expected: Vec<String> = expected
                    .iter()
                    .map(|n| {
                        format!(
                            "NDFileHDF5::writeHdfAttributes unknown type: \
                             unable to create attribute: {n}\n"
                        )
                    })
                    .collect();
                assert_eq!(
                    writer.open_posts.lock().lines,
                    expected,
                    "swmr={swmr} store={store}"
                );
                writer.close_file().unwrap();
            }
        }
    }

    /// A detector dataset under a group C refuses lands on that group's
    /// parent, and frames go there (C oracle).
    #[test]
    fn a_detector_dataset_under_a_refused_group_takes_the_frames() {
        for swmr in [false, true] {
            let file = write_frames_through_layout(
                swmr,
                "refused_parent",
                r#"<hdf5_layout><group name="entry">
                  <group ndattr_default="true"><dataset name="data" source="detector"/></group>
                </group></hdf5_layout>"#,
                2,
            );
            let h5 = H5File::open(&file).unwrap();
            assert_eq!(
                h5.dataset("entry/data").unwrap().shape(),
                vec![2, 4],
                "swmr={swmr}"
            );
            assert!(h5.dataset("exposure").is_ok(), "swmr={swmr}");
            std::fs::remove_file(&file).ok();
        }
    }

    /// C refuses a layout without a detector dataset once `createTree` has
    /// built the rest, and that tree stays in the file (NDFileHDF5.cpp:463-491,
    /// C oracle). A root detector dataset counts for none.
    #[test]
    fn a_layout_without_a_detector_dataset_refuses_the_open_after_the_tree() {
        let dir = tempfile::tempdir().unwrap();
        let layouts = [
            r#"<hdf5_layout><group name="entry">
              <attribute name="NX_class" source="constant" value="NXentry" type="string"/>
              <attribute name="nosrc"/>
              <dataset name="c" source="constant" value="2" type="int"/>
              <dataset name="exp" source="ndattribute" ndattribute="exposure"/>
              <group name="NDAttributes" ndattr_default="true"/>
            </group></hdf5_layout>"#,
            r#"<hdf5_layout><group name="entry">
              <dataset name="c" source="constant" value="2" type="int"/></group>
              <dataset name="data" source="detector" det_default="true"/>
            </hdf5_layout>"#,
        ];
        let refusal = [
            "NDFileHDF5::createXMLFileLayout No detector datasets found, cannot continue\n",
            "NDFileHDF5::openFile ERROR Failed to create the specified file layout\n",
        ];
        for (i, layout) in layouts.into_iter().enumerate() {
            let xml = dir.path().join(format!("nodet{i}.xml"));
            std::fs::write(&xml, layout).unwrap();
            for swmr in [false, true] {
                let path = dir.path().join(format!("nodet{i}_{swmr}.h5"));
                let mut writer = Hdf5Writer::new();
                writer.set_swmr_mode(swmr);
                writer.set_store_attributes(true);
                writer.set_layout_filename(xml.to_str().unwrap());
                writer.hostname = "testhost".into();
                let mut arr = NDArray::new(vec![NDDimension::new(4)], NDDataType::UInt8);
                arr.attributes.add(NDAttribute::new_static(
                    "exposure",
                    "",
                    NDAttrSource::Driver,
                    NDAttrValue::Float64(0.5),
                ));
                let err = writer
                    .open_file(&path, NDFileOpenMode::WRITE, &arr)
                    .unwrap_err();
                assert_eq!(err.lines(), refusal, "layout {i} swmr={swmr}");
                let unknown: &[&str] = if i == 0 {
                    &[
                        "NDFileHDF5::writeHdfAttributes unknown type: unable to create attribute: nosrc\n",
                    ]
                } else {
                    &[]
                };
                assert_eq!(
                    writer.open_posts.lock().lines,
                    unknown,
                    "layout {i} swmr={swmr}"
                );
                assert!(writer.handle.is_none(), "layout {i} swmr={swmr}");
                let h5 = H5File::open(&path).unwrap();
                assert_eq!(
                    h5.dataset("entry/c").unwrap().read_raw::<i32>().unwrap(),
                    vec![2],
                    "layout {i} swmr={swmr}"
                );
                let names = h5.dataset_names();
                assert_eq!(names, vec!["entry/c".to_owned()], "layout {i} swmr={swmr}");
                if i == 0 {
                    let entry = h5.root_group().group("entry").unwrap();
                    assert_eq!(entry.attr_string("NX_class").unwrap(), "NXentry");
                    // createAttributeDataset never ran, so no host is named.
                    let ndattrs = entry.group("NDAttributes").unwrap();
                    assert!(
                        !ndattrs
                            .attr_names()
                            .unwrap()
                            .contains(&"hostname".to_owned()),
                        "swmr={swmr}"
                    );
                }
            }
        }
    }

    /// C `openFile` creates the layout tree and the detector datasets
    /// (NDFileHDF5.cpp:270-278), so a file closed before any frame holds them.
    fn assert_zero_frame_file_holds_the_layout(swmr: bool) {
        let file = write_frames_through_layout(
            swmr,
            "zero_frames",
            r#"<hdf5_layout><group name="entry">
              <attribute name="kind" source="constant" value="7" type="int"/>
              <dataset name="data" source="detector" det_default="true"/>
              <group name="NDAttributes" ndattr_default="true"/>
            </group></hdf5_layout>"#,
            0,
        );
        let h5 = H5File::open(&file).unwrap();
        assert!(h5.dataset("entry/data").is_ok(), "swmr={swmr}");
        assert!(!attr_messages(&file, "kind").is_empty(), "swmr={swmr}");
        std::fs::remove_file(&file).ok();
    }

    /// C's frame axis starts at extent 1 and the first write fills that slot
    /// (NDFileHDF5AttributeDataset.cpp:255, NDFileHDF5Dataset.cpp:89), so a file
    /// closed before any frame holds one fill element per dataset.
    fn assert_zero_frame_datasets_hold_one_fill_element(swmr: bool) {
        let file = write_frames_through_layout(
            swmr,
            "zero_extent",
            r#"<hdf5_layout><group name="entry">
              <dataset name="data" source="detector" det_default="true"/>
              <group name="NDAttributes" ndattr_default="true"/>
            </group></hdf5_layout>"#,
            0,
        );
        let h5 = H5File::open(&file).unwrap();
        assert_eq!(
            h5.dataset("entry/data").unwrap().shape(),
            vec![1, 4],
            "swmr={swmr}"
        );
        let exposure = h5.dataset("entry/NDAttributes/exposure").unwrap();
        assert_eq!(exposure.shape(), vec![1], "swmr={swmr}");
        assert_eq!(
            exposure.read_raw::<f64>().unwrap(),
            vec![0.0],
            "swmr={swmr}"
        );
        std::fs::remove_file(&file).ok();
    }

    #[test]
    fn zero_frame_datasets_hold_one_fill_element() {
        assert_zero_frame_datasets_hold_one_fill_element(false);
    }

    #[test]
    fn swmr_zero_frame_datasets_hold_one_fill_element() {
        assert_zero_frame_datasets_hold_one_fill_element(true);
    }

    /// C creates `timestamp` at open and sets its extent to the captured
    /// rows at close (NDFileHDF5.cpp:2596-2700), so a file closed before any
    /// frame holds it with zero rows.
    fn assert_zero_frame_file_holds_an_empty_timestamp(swmr: bool) {
        let file = write_frames_through_layout(
            swmr,
            "zero_perf",
            r#"<hdf5_layout><group name="entry">
              <dataset name="data" source="detector" det_default="true"/>
              <group name="NDAttributes" ndattr_default="true"/>
            </group></hdf5_layout>"#,
            0,
        );
        let h5 = H5File::open(&file).unwrap();
        assert_eq!(
            h5.dataset("entry/NDAttributes/timestamp").unwrap().shape(),
            vec![0, 5],
            "swmr={swmr}"
        );
        std::fs::remove_file(&file).ok();
    }

    #[test]
    fn zero_frame_file_holds_an_empty_timestamp() {
        assert_zero_frame_file_holds_an_empty_timestamp(false);
    }

    #[test]
    fn swmr_zero_frame_file_holds_an_empty_timestamp() {
        assert_zero_frame_file_holds_an_empty_timestamp(true);
    }

    /// An NDAttribute named `timestamp` takes that name in the performance
    /// group first, so C's `createPerformanceDataset` fails, only warns and
    /// drops the performance rows (NDFileHDF5.cpp:313-316, :2661-2668); the
    /// rest of the file — hardlinks, OnFileClose attributes — is written.
    fn assert_timestamp_ndattribute_drops_only_the_perf_rows(swmr: bool) {
        let xml = std::env::temp_dir().join(format!(
            "adcore_layout_perf_clash_{}_{swmr}.xml",
            std::process::id()
        ));
        std::fs::write(
            &xml,
            r#"<hdf5_layout><group name="entry">
              <attribute name="AtClose" source="ndattribute" ndattribute="timestamp" when="OnFileClose"/>
              <dataset name="data" source="detector" det_default="true"/>
              <group name="NDAttributes" ndattr_default="true"/>
              <hardlink name="link" target="/entry/data"/>
            </group></hdf5_layout>"#,
        )
        .unwrap();
        let path = temp_path(&format!("hdf5_perf_clash_{swmr}"));
        let mut writer = Hdf5Writer::new();
        writer.set_swmr_mode(swmr);
        writer.set_num_capture(0);
        writer.set_store_attributes(true);
        writer.set_store_performance(true);
        writer.set_layout_filename(xml.to_str().unwrap());
        let mk = |t: f64| {
            let mut arr = NDArray::new(vec![NDDimension::new(4)], NDDataType::UInt8);
            arr.attributes.add(NDAttribute::new_static(
                "timestamp",
                "",
                NDAttrSource::Driver,
                NDAttrValue::Float64(t),
            ));
            arr
        };
        writer
            .open_file(
                &path,
                NDFileOpenMode::WRITE | NDFileOpenMode::MULTIPLE,
                &mk(0.5),
            )
            .unwrap();
        writer.write_file(&Arc::new(mk(1.5))).unwrap();
        writer.write_file(&Arc::new(mk(2.5))).unwrap();
        writer.close_file().unwrap();
        std::fs::remove_file(&xml).ok();

        let h5 = H5File::open(&path).unwrap();
        let ts = h5.dataset("entry/NDAttributes/timestamp").unwrap();
        assert_eq!(ts.read_raw::<f64>().unwrap(), [1.5, 2.5], "swmr={swmr}");
        assert_eq!(h5.dataset("entry/link").unwrap().shape(), vec![2, 4]);
        let entry = h5.root_group().group("entry").unwrap();
        assert!(entry.attr_names().unwrap().contains(&"AtClose".to_owned()));
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn timestamp_ndattribute_drops_only_the_perf_rows() {
        assert_timestamp_ndattribute_drops_only_the_perf_rows(false);
    }

    #[test]
    fn swmr_timestamp_ndattribute_drops_only_the_perf_rows() {
        assert_timestamp_ndattribute_drops_only_the_perf_rows(true);
    }

    /// Writes `frames` 4x4 frames carrying `AcqT` = 1.0, 2.0, … through a
    /// writer with `HDF5_dimAttDatasets` set, opened on a frame with `AcqT`
    /// 0.5; returns the file.
    fn write_dim_att_file(
        name: &str,
        configure: impl FnOnce(&mut Hdf5Writer),
        mode: NDFileOpenMode,
        frames: usize,
    ) -> std::path::PathBuf {
        let xml =
            std::env::temp_dir().join(format!("adcore_layout_{name}_{}.xml", std::process::id()));
        std::fs::write(
            &xml,
            r#"<hdf5_layout><group name="entry">
              <dataset name="data" source="detector" det_default="true"/>
              <group name="NDAttributes" ndattr_default="true"/>
            </group></hdf5_layout>"#,
        )
        .unwrap();
        let path = temp_path(name);
        let mut writer = Hdf5Writer::new();
        writer.set_store_attributes(true);
        writer.set_dim_att_datasets(true);
        writer.set_layout_filename(xml.to_str().unwrap());
        configure(&mut writer);
        let mk = |v: f64| {
            let mut arr = NDArray::new(
                vec![NDDimension::new(4), NDDimension::new(4)],
                NDDataType::UInt8,
            );
            arr.attributes.add(NDAttribute::new_static(
                "AcqT",
                "",
                NDAttrSource::Driver,
                NDAttrValue::Float64(v),
            ));
            arr
        };
        writer.open_file(&path, mode, &mk(0.5)).unwrap();
        for f in 1..=frames {
            writer.write_file(&Arc::new(mk(f as f64))).unwrap();
        }
        writer.close_file().unwrap();
        std::fs::remove_file(&xml).ok();
        path
    }

    /// `HDF5_dimAttDatasets` gives each NDAttribute dataset the extra-dimension
    /// grid, filled in the detector's odometer order and chunked by the
    /// extra-dimension sizes (NDFileHDF5.cpp:2803-2813, :2869-2920). C oracle:
    /// five frames on X=3, N=2 read `[[1,2],[3,4],[5,0]]`, chunk `[3,2]`.
    fn assert_dim_att_datasets_follow_the_extra_dimension_grid(swmr: bool) {
        let path = write_dim_att_file(
            &format!("hdf5_dimatt_grid_{swmr}"),
            |w| {
                w.set_swmr_mode(swmr);
                // Each frame's flush rewrites the grid's one chunk in place.
                w.set_flush_nth_frame(1);
                w.set_n_extra_dims(1);
                w.set_extra_dim_size(0, 2);
                w.set_extra_dim_size(1, 3);
                w.set_num_capture(6);
            },
            NDFileOpenMode::WRITE | NDFileOpenMode::MULTIPLE,
            5,
        );
        let h5 = H5File::open(&path).unwrap();
        let ds = h5.dataset("entry/NDAttributes/AcqT").unwrap();
        assert_eq!(ds.shape(), vec![3, 2], "swmr={swmr}");
        assert_eq!(ds.chunk_dims(), Some(vec![3, 2]), "swmr={swmr}");
        assert_eq!(
            ds.read_raw::<f64>().unwrap(),
            [1.0, 2.0, 3.0, 4.0, 5.0, 0.0],
            "swmr={swmr}"
        );
        assert_eq!(ds.max_shape().unwrap(), vec![None, None], "swmr={swmr}");
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn dim_att_datasets_follow_the_extra_dimension_grid() {
        assert_dim_att_datasets_follow_the_extra_dimension_grid(false);
    }

    #[test]
    fn swmr_dim_att_datasets_follow_the_extra_dimension_grid() {
        assert_dim_att_datasets_follow_the_extra_dimension_grid(true);
    }

    /// With no extra dimension a `HDF5_dimAttDatasets` dataset stays 1-D but
    /// chunks by `HDF5_extraDimSizeN`, or 32 for an unlimited capture
    /// (NDFileHDF5.cpp:2885-2909). C oracle: N=2 → `[2]`, NumCapture 0 → `[32]`.
    fn assert_dim_att_series_chunks_by_the_n_dimension(swmr: bool) {
        for (num_capture, chunk) in [(3, 2), (0, 32)] {
            let path = write_dim_att_file(
                &format!("hdf5_dimatt_n_{swmr}_{num_capture}"),
                |w| {
                    w.set_swmr_mode(swmr);
                    w.set_extra_dim_size(0, 2);
                    w.set_num_capture(num_capture);
                },
                NDFileOpenMode::WRITE | NDFileOpenMode::MULTIPLE,
                3,
            );
            let h5 = H5File::open(&path).unwrap();
            let ds = h5.dataset("entry/NDAttributes/AcqT").unwrap();
            assert_eq!(ds.read_raw::<f64>().unwrap(), [1.0, 2.0, 3.0]);
            assert_eq!(
                ds.chunk_dims(),
                Some(vec![chunk]),
                "swmr={swmr} num_capture={num_capture}"
            );
            std::fs::remove_file(&path).ok();
        }
    }

    #[test]
    fn dim_att_series_chunks_by_the_n_dimension() {
        assert_dim_att_series_chunks_by_the_n_dimension(false);
    }

    #[test]
    fn swmr_dim_att_series_chunks_by_the_n_dimension() {
        assert_dim_att_series_chunks_by_the_n_dimension(true);
    }

    /// In a Single file `HDF5_dimAttDatasets` gives C's rank-0 dataset, as the
    /// frame carries no frame axis (`configureDimsFromDataset` with
    /// `multiframe` false; C oracle: `AcqT` shape `()`).
    fn assert_dim_att_datasets_in_a_single_file_are_scalar(swmr: bool) {
        let path = write_dim_att_file(
            &format!("hdf5_dimatt_single_{swmr}"),
            |w| w.set_swmr_mode(swmr),
            NDFileOpenMode::WRITE,
            1,
        );
        let h5 = H5File::open(&path).unwrap();
        let ds = h5.dataset("entry/NDAttributes/AcqT").unwrap();
        assert_eq!(ds.shape(), Vec::<usize>::new(), "swmr={swmr}");
        assert_eq!(ds.read_raw::<f64>().unwrap(), [1.0], "swmr={swmr}");
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn dim_att_datasets_in_a_single_file_are_scalar() {
        assert_dim_att_datasets_in_a_single_file_are_scalar(false);
    }

    #[test]
    fn swmr_dim_att_datasets_in_a_single_file_are_scalar() {
        assert_dim_att_datasets_in_a_single_file_are_scalar(true);
    }

    /// C writes and flushes NDArrayUniqueId after every other attribute
    /// dataset (NDFileHDF5.cpp:355-390, :2955-2992); rust-hdf5 publishes SWMR
    /// datasets in creation order, so its dataset is created last.
    fn assert_the_unique_id_attribute_dataset_comes_last(swmr: bool) {
        let path = temp_path(&format!("hdf5_uid_last_{swmr}"));
        let mut writer = Hdf5Writer::new();
        writer.set_swmr_mode(swmr);
        writer.set_store_attributes(true);
        let mut arr = NDArray::new(vec![NDDimension::new(4)], NDDataType::UInt8);
        arr.attributes.add(NDAttribute::new_static(
            "AcqT",
            "",
            NDAttrSource::Driver,
            NDAttrValue::Float64(0.1),
        ));
        writer
            .open_file(
                &path,
                NDFileOpenMode::WRITE | NDFileOpenMode::MULTIPLE,
                &arr,
            )
            .unwrap();
        let names: Vec<&str> = writer
            .attr_datasets
            .iter()
            .map(|ad| ad.name.as_str())
            .collect();
        assert_eq!(
            names.last(),
            Some(&"NDArrayUniqueId"),
            "swmr={swmr} {names:?}"
        );
        if swmr {
            let created: Vec<usize> = writer
                .attr_datasets
                .iter()
                .map(|ad| match ad.sink {
                    AttrSink::Swmr(i)
                    | AttrSink::SwmrGrid { index: i, .. }
                    | AttrSink::SwmrScalar(i) => i,
                    AttrSink::Buffer(_) => unreachable!("a SWMR file buffers no attribute"),
                })
                .collect();
            assert!(created.is_sorted(), "{created:?}");
        }
        writer.close_file().unwrap();
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn the_unique_id_attribute_dataset_comes_last() {
        assert_the_unique_id_attribute_dataset_comes_last(false);
    }

    #[test]
    fn swmr_the_unique_id_attribute_dataset_comes_last() {
        assert_the_unique_id_attribute_dataset_comes_last(true);
    }

    /// C refuses a negative capture count, then still verifies the layout,
    /// and creates no file (NDFileHDF5.cpp:205-217).
    #[test]
    fn open_refuses_a_negative_num_capture() {
        let path = temp_path("hdf5_negative_num_capture");
        let arr = NDArray::new(vec![NDDimension::new(4)], NDDataType::UInt8);
        let invalid = "NDFileHDF5::openFile Invalid number of frames to capture: -1. \
                       Please specify a number >= 0\n";
        let bad_layout =
            "NDFileHDF5::verifyLayoutXMLFile XML description file could not be opened.\n";
        for (layout, want) in [
            ("", vec![invalid]),
            ("/nonexistent/l.xml", vec![invalid, bad_layout]),
        ] {
            let mut writer = Hdf5Writer::new();
            writer.set_num_capture(-1);
            writer.set_layout_filename(layout);
            let r = writer.open_file(
                &path,
                NDFileOpenMode::WRITE | NDFileOpenMode::MULTIPLE,
                &arr,
            );
            let Err(ADError::Reported { lines, .. }) = r else {
                panic!("layout {layout:?}: {r:?}");
            };
            assert_eq!(lines, want, "layout {layout:?}");
            assert!(!path.exists(), "layout {layout:?}");
        }
    }

    /// Under `HDF5_dimAttDatasets` a frame `detector_data_destination` sends
    /// to another dataset writes no OnFrame element (NDFileHDF5.cpp:1561-1567).
    #[test]
    fn dim_att_datasets_skip_a_frame_sent_elsewhere() {
        let xml = std::env::temp_dir().join(format!(
            "adcore_layout_dimatt_dest_{}.xml",
            std::process::id()
        ));
        std::fs::write(
            &xml,
            r#"<hdf5_layout>
              <global name="detector_data_destination" ndattribute="dest"/>
              <group name="entry">
                <dataset name="data1" source="detector" det_default="true"/>
                <dataset name="data2" source="detector"/>
                <group name="NDAttributes" ndattr_default="true"/>
              </group>
            </hdf5_layout>"#,
        )
        .unwrap();
        for (dim_att, want) in [(true, vec![1.0, 3.0]), (false, vec![1.0, 2.0, 3.0])] {
            let path = temp_path(&format!("hdf5_dimatt_dest_{dim_att}"));
            let mut writer = Hdf5Writer::new();
            writer.set_num_capture(3);
            writer.set_store_attributes(true);
            writer.set_dim_att_datasets(dim_att);
            writer.set_layout_filename(xml.to_str().unwrap());
            let mk = |v: f64, dest: &str| {
                let mut arr = NDArray::new(vec![NDDimension::new(4)], NDDataType::UInt8);
                arr.attributes.add(NDAttribute::new_static(
                    "AcqT",
                    "",
                    NDAttrSource::Driver,
                    NDAttrValue::Float64(v),
                ));
                arr.attributes.add(NDAttribute::new_static(
                    "dest",
                    "",
                    NDAttrSource::Driver,
                    NDAttrValue::String(dest.into()),
                ));
                arr
            };
            writer
                .open_file(
                    &path,
                    NDFileOpenMode::WRITE | NDFileOpenMode::MULTIPLE,
                    &mk(0.5, ""),
                )
                .unwrap();
            for (v, dest) in [(1.0, ""), (2.0, "/entry/data2"), (3.0, "")] {
                writer.write_file(&Arc::new(mk(v, dest))).unwrap();
            }
            writer.close_file().unwrap();
            let h5 = H5File::open(&path).unwrap();
            let ds = h5.dataset("entry/NDAttributes/AcqT").unwrap();
            assert_eq!(ds.read_raw::<f64>().unwrap(), want, "dim_att={dim_att}");
            std::fs::remove_file(&path).ok();
        }
        std::fs::remove_file(&xml).ok();
    }

    /// Write UInt8 4x4 frames `1, 2, …` carrying `PosX`/`PosN` Int32
    /// NDAttributes, in PositionMode; each write's result is returned.
    fn write_positioned(
        name: &str,
        swmr: bool,
        configure: impl FnOnce(&mut Hdf5Writer),
        positions: &[(i32, i32)],
    ) -> (std::path::PathBuf, Vec<ADResult<()>>) {
        let xml =
            std::env::temp_dir().join(format!("adcore_layout_{name}_{}.xml", std::process::id()));
        std::fs::write(
            &xml,
            r#"<hdf5_layout><group name="entry">
              <dataset name="data" source="detector" det_default="true"/>
              <group name="NDAttributes" ndattr_default="true"/>
            </group></hdf5_layout>"#,
        )
        .unwrap();
        let path = temp_path(name);
        let mut writer = Hdf5Writer::new();
        writer.set_layout_filename(xml.to_str().unwrap());
        writer.set_swmr_mode(swmr);
        writer.set_pos_running(true);
        writer.set_pos_name(0, "PosN");
        writer.set_pos_name(1, "PosX");
        configure(&mut writer);
        let mk = |v: u8, (x, n): (i32, i32)| {
            let mut arr = NDArray::with_data(
                vec![NDDimension::new(4), NDDimension::new(4)],
                NDDataBuffer::from_vec::<u8>(vec![v; 16]),
            );
            for (name, pos) in [("PosX", x), ("PosN", n)] {
                arr.attributes.add(NDAttribute::new_static(
                    name,
                    "",
                    NDAttrSource::Driver,
                    NDAttrValue::Int32(pos),
                ));
            }
            arr
        };
        writer
            .open_file(
                &path,
                NDFileOpenMode::WRITE | NDFileOpenMode::MULTIPLE,
                &mk(0, (0, 0)),
            )
            .unwrap();
        let mut results = Vec::new();
        for (i, &pos) in positions.iter().enumerate() {
            let r = writer.write_file(&Arc::new(mk(i as u8 + 1, pos)));
            let aborted = r.as_ref().is_err_and(|e| e.is_aborted());
            results.push(r);
            if aborted {
                break;
            }
        }
        if !results.last().is_some_and(|r| r.is_err()) {
            writer.close_file().unwrap();
        }
        std::fs::remove_file(&xml).ok();
        (path, results)
    }

    /// The data's shape and each frame cell's pixel value, row-major.
    fn positioned_cells(path: &std::path::Path) -> (Vec<usize>, Vec<u8>) {
        let h5 = H5File::open(path).unwrap();
        let ds = h5.dataset("entry/data").unwrap();
        let cells = ds
            .read_raw::<u8>()
            .unwrap()
            .chunks(16)
            .map(|c| c[0])
            .collect();
        (ds.shape(), cells)
    }

    /// PositionMode places each frame where its `PosNameDim*` NDAttributes
    /// say and grows each extra dimension to the furthest position
    /// (NDFileHDF5.cpp:1480-1517, NDFileHDF5Dataset.cpp:161-185). C oracle:
    /// (x, n) = (2,1) (0,0) (1,1) (0,1) on X=3, N=2 give shape (3,2,4,4).
    fn assert_positions_place_frames_on_the_extra_dimensions(swmr: bool) {
        let (path, results) = write_positioned(
            &format!("hdf5_pos_grid_{swmr}"),
            swmr,
            |w| {
                w.set_n_extra_dims(1);
                w.set_extra_dim_size(0, 2);
                w.set_extra_dim_size(1, 3);
                w.set_num_capture(6);
            },
            &[(2, 1), (0, 0), (1, 1), (0, 1)],
        );
        assert!(results.iter().all(Result::is_ok), "swmr={swmr}");
        assert_eq!(
            positioned_cells(&path),
            (vec![3, 2, 4, 4], vec![2, 4, 0, 3, 0, 1]),
            "swmr={swmr}"
        );
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn positions_place_frames_on_the_extra_dimensions() {
        assert_positions_place_frames_on_the_extra_dimensions(false);
    }

    #[test]
    fn swmr_positions_place_frames_on_the_extra_dimensions() {
        assert_positions_place_frames_on_the_extra_dimensions(true);
    }

    /// Each extra axis chunks at its `extraDimChunk` clamped to its size, the
    /// innermost at nFramesChunks (NDFileHDF5.cpp:3184-3274), and frames
    /// sharing a chunk land in any order. C oracle: X chunk 2 on X=3 gives
    /// (2,1,4,4); Y chunk 2, X chunk 5, nFramesChunks 2 on (2,3,2) give
    /// (2,3,2,4,4).
    fn assert_extra_dim_chunks_follow_c(swmr: bool) {
        let x_chunk = |w: &mut Hdf5Writer| {
            w.set_n_extra_dims(1);
            w.set_extra_dim_size(0, 2);
            w.set_extra_dim_size(1, 3);
            w.set_extra_dim_chunk(1, 2);
            w.set_num_capture(6);
        };
        let cases: [(
            &str,
            Box<dyn FnOnce(&mut Hdf5Writer)>,
            &[(i32, i32)],
            &[usize],
            &[u8],
        ); 3] = [
            (
                "seq",
                Box::new(move |w: &mut Hdf5Writer| {
                    x_chunk(w);
                    w.set_pos_running(false);
                }),
                &[(0, 0); 6],
                &[2, 1, 4, 4],
                &[1, 2, 3, 4, 5, 6],
            ),
            (
                "pos",
                Box::new(move |w: &mut Hdf5Writer| {
                    x_chunk(w);
                    w.set_n_frames_chunks(2);
                }),
                &[(2, 1), (0, 0), (1, 1), (0, 1)],
                &[2, 2, 4, 4],
                &[2, 4, 0, 3, 0, 1],
            ),
            (
                "yx",
                Box::new(|w: &mut Hdf5Writer| {
                    w.set_pos_running(false);
                    w.set_n_extra_dims(2);
                    w.set_extra_dim_size(0, 2);
                    w.set_extra_dim_size(1, 3);
                    w.set_extra_dim_size(2, 2);
                    w.set_extra_dim_chunk(1, 5);
                    w.set_extra_dim_chunk(2, 2);
                    w.set_n_frames_chunks(2);
                    w.set_num_capture(12);
                }),
                &[(0, 0); 7],
                &[2, 3, 2, 4, 4],
                &[1, 2, 3, 4, 5, 6, 7, 0, 0, 0, 0, 0],
            ),
        ];
        for (name, configure, positions, chunk, cells) in cases {
            let (path, results) = write_positioned(
                &format!("hdf5_ed_chunk_{name}_{swmr}"),
                swmr,
                configure,
                positions,
            );
            assert!(results.iter().all(Result::is_ok), "{name} swmr={swmr}");
            let h5 = H5File::open(&path).unwrap();
            let ds = h5.dataset("entry/data").unwrap();
            assert_eq!(ds.chunk_dims().unwrap(), chunk, "{name} swmr={swmr}");
            assert_eq!(positioned_cells(&path).1, cells, "{name} swmr={swmr}");
            std::fs::remove_file(&path).ok();
        }
    }

    #[test]
    fn extra_dim_chunks_follow_c() {
        assert_extra_dim_chunks_follow_c(false);
    }

    #[test]
    fn swmr_extra_dim_chunks_follow_c() {
        assert_extra_dim_chunks_follow_c(true);
    }

    /// With no extra dimensions the position is the frame index, bounded by
    /// `extraDimSizeN`, and frames sharing an `nFramesChunks`-deep chunk land
    /// in any order (C oracle: N = 2, 0, 1 on N=3 give (3,4,4)). The frame
    /// axis is NumCapture deep, unlimited when that is 0, and chunked at
    /// nFramesChunks on both paths (C oracle: (2,4,4) in a SWMR file).
    fn assert_positions_place_frames_on_the_frame_axis(swmr: bool, frames_chunks: usize) {
        for (num_capture, max) in [(3, Some(3)), (0, None)] {
            for (positions, want) in [
                (&[2, 0, 1][..], (vec![3, 4, 4], vec![2, 3, 1])),
                (&[1][..], (vec![2, 4, 4], vec![0, 1])),
            ] {
                let positions: Vec<(i32, i32)> = positions.iter().map(|&n| (0, n)).collect();
                let (path, results) = write_positioned(
                    &format!(
                        "hdf5_pos_series_{swmr}_{frames_chunks}_{num_capture}_{}",
                        positions.len()
                    ),
                    swmr,
                    |w| {
                        w.set_extra_dim_size(0, 3);
                        w.set_num_capture(num_capture);
                        w.set_n_frames_chunks(frames_chunks);
                    },
                    &positions,
                );
                let at = format!("swmr={swmr} nc={num_capture} {positions:?}");
                assert!(results.iter().all(Result::is_ok), "{at}");
                assert_eq!(positioned_cells(&path), want, "{at}");
                let h5 = H5File::open(&path).unwrap();
                let ds = h5.dataset("entry/data").unwrap();
                assert_eq!(ds.max_shape().unwrap(), [max, Some(4), Some(4)], "{at}");
                assert_eq!(ds.chunk_dims().unwrap(), [frames_chunks, 4, 4], "{at}");
                std::fs::remove_file(&path).ok();
            }
        }
    }

    #[test]
    fn positions_place_frames_on_the_frame_axis() {
        assert_positions_place_frames_on_the_frame_axis(false, 1);
        assert_positions_place_frames_on_the_frame_axis(false, 2);
    }

    #[test]
    fn swmr_positions_place_frames_on_the_frame_axis() {
        assert_positions_place_frames_on_the_frame_axis(true, 1);
        assert_positions_place_frames_on_the_frame_axis(true, 2);
    }

    /// A position past its extra dimension, or a missing position attribute,
    /// aborts the file with C's lines; the frames before it stay (C oracle:
    /// (1,0) then (3,0) on X=3 leave (2,1,4,4); a missing one leaves the
    /// initial (1,1,4,4)).
    #[test]
    fn a_bad_position_aborts_the_file() {
        let abort = "NDFileHDF5::writeFile ERROR: could not write to dataset. Aborting\n";
        for (name, pos_n, positions, line, want) in [
            (
                "past",
                "PosN",
                &[(1, 0), (3, 0)][..],
                "NDFileHDF5Dataset::extendDataSet ERROR extending the dataset [data] failed\n",
                (vec![2, 1, 4, 4], vec![0, 1]),
            ),
            (
                "missing",
                "Nope",
                &[(1, 0)][..],
                "NDFileHDF5::findPositionIndex ERROR: could not find position attribute \
                 Nope in list. Aborting\n",
                (vec![1, 1, 4, 4], vec![0]),
            ),
        ] {
            let (path, results) = write_positioned(
                &format!("hdf5_pos_bad_{name}"),
                false,
                |w| {
                    w.set_n_extra_dims(1);
                    w.set_extra_dim_size(0, 2);
                    w.set_extra_dim_size(1, 3);
                    w.set_num_capture(6);
                    w.set_pos_name(0, pos_n);
                },
                positions,
            );
            let err = results.last().unwrap().as_ref().unwrap_err();
            assert!(err.is_aborted(), "{name}");
            assert_eq!(err.lines(), [line, abort], "{name}");
            assert_eq!(positioned_cells(&path), want, "{name}");
            std::fs::remove_file(&path).ok();
        }
    }

    /// C creates `timestamp` at open and its abort closes without
    /// `writePerformanceDataset`, so the aborted file holds it unwritten (C
    /// oracle: (1,5), maxshape (None,None), chunk (6,5), fill 0).
    #[test]
    fn an_aborted_file_keeps_the_unwritten_performance_dataset() {
        for swmr in [false, true] {
            let (path, results) = write_positioned(
                &format!("hdf5_pos_abort_perf_{swmr}"),
                swmr,
                |w| {
                    w.set_store_performance(true);
                    w.set_n_extra_dims(1);
                    w.set_extra_dim_size(0, 2);
                    w.set_extra_dim_size(1, 3);
                    w.set_num_capture(6);
                },
                &[(1, 0), (3, 0)],
            );
            let err = results.last().unwrap().as_ref().unwrap_err();
            assert!(err.is_aborted(), "swmr={swmr}");
            let h5 = H5File::open(&path).unwrap();
            let ds = h5.dataset("entry/NDAttributes/timestamp").unwrap();
            assert_eq!(ds.shape(), [1, 5], "swmr={swmr}");
            assert_eq!(ds.max_shape().unwrap(), [None, None], "swmr={swmr}");
            assert_eq!(ds.chunk_dims().unwrap(), [6, 5], "swmr={swmr}");
            assert_eq!(ds.read_raw::<f64>().unwrap(), [0.0; 5], "swmr={swmr}");
            std::fs::remove_file(&path).ok();
        }
    }

    /// Under HDF5_dimAttDatasets an attribute goes where its frame goes, and
    /// the one `HDF5_posIndexDimX` names is a series of the X offsets (C
    /// oracle). With no extra dimensions the series axis is N. Every axis is
    /// unlimited and an index series chunks by `calculateAttributeChunking`
    /// (NDFileHDF5AttributeDataset.cpp:301, NDFileHDF5.cpp:2845).
    fn assert_positions_place_attributes(swmr: bool) {
        let attrs = |w: &mut Hdf5Writer| {
            w.set_store_attributes(true);
            w.set_dim_att_datasets(true);
            w.set_pos_index_name(1, "PosX");
        };
        let (path, results) = write_positioned(
            &format!("hdf5_pos_attrs_{swmr}"),
            swmr,
            |w| {
                attrs(w);
                w.set_n_extra_dims(1);
                w.set_extra_dim_size(0, 2);
                w.set_extra_dim_size(1, 3);
                w.set_num_capture(6);
            },
            &[(2, 1), (0, 0), (1, 1), (0, 1)],
        );
        assert!(results.iter().all(Result::is_ok), "swmr={swmr}");
        let read = |path: &std::path::Path, name: &str| {
            let h5 = H5File::open(path).unwrap();
            let ds = h5.dataset(&format!("entry/NDAttributes/{name}")).unwrap();
            let max = ds.max_shape().unwrap();
            assert!(
                max.iter().all(Option::is_none),
                "swmr={swmr} {name} {max:?}"
            );
            (
                ds.shape(),
                ds.chunk_dims().unwrap(),
                ds.read_raw::<i32>().unwrap(),
            )
        };
        assert_eq!(
            read(&path, "PosN"),
            (vec![3, 2], vec![3, 2], vec![0, 1, 0, 1, 0, 1]),
            "swmr={swmr}"
        );
        assert_eq!(
            read(&path, "PosX"),
            (vec![3], vec![6], vec![0, 1, 2]),
            "swmr={swmr}"
        );
        std::fs::remove_file(&path).ok();

        let (path, results) = write_positioned(
            &format!("hdf5_pos_attrs_n_{swmr}"),
            swmr,
            |w| {
                attrs(w);
                w.set_extra_dim_size(0, 3);
                w.set_num_capture(3);
            },
            &[(0, 2), (0, 0), (0, 1)],
        );
        assert!(results.iter().all(Result::is_ok), "swmr={swmr}");
        assert_eq!(
            read(&path, "PosN"),
            (vec![3], vec![3], vec![0, 1, 2]),
            "swmr={swmr}"
        );
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn positions_place_attributes() {
        assert_positions_place_attributes(false);
    }

    #[test]
    fn swmr_positions_place_attributes() {
        assert_positions_place_attributes(true);
    }

    /// PositionMode, PosNameDim* and PosIndexDim* reach the writer.
    #[test]
    fn position_params_reach_the_settings() {
        let (proc, mut base) = hdf5_port();
        for (name, v) in [("HDF5_posNameDimX", "PosX"), ("HDF5_posIndexDimY", "PosY")] {
            proc.validate_octet(
                idx(&base, name),
                0,
                v,
                WriteVerdict::Accept(v.to_owned()),
                &base,
            );
        }
        proc.validate_int32(
            idx(&base, "HDF5_posRunning"),
            0,
            1,
            WriteVerdict::Accept(1),
            &mut ad_core_rs::plugin::runtime::WritePort::new(&mut base),
        );
        assert!(proc.settings.get(|s| s.pos_running));
        assert_eq!(proc.settings.get(|s| s.pos_names[1].clone()), "PosX");
        assert_eq!(proc.settings.get(|s| s.pos_index_names[2].clone()), "PosY");
    }

    #[test]
    fn extra_dim_chunk_reaches_the_settings() {
        let (proc, mut base) = hdf5_port();
        proc.validate_int32(
            idx(&base, "HDF5_extraDimChunkX"),
            0,
            2,
            WriteVerdict::Accept(2),
            &mut ad_core_rs::plugin::runtime::WritePort::new(&mut base),
        );
        assert_eq!(proc.settings.get(|s| s.extra_dims[1].chunk), 2);
    }

    /// An extra-dimension grid starts at C's `dims_`, 1 on each leading
    /// axis, so a file closed before any frame holds (1,1,4,4) (C oracle: a
    /// file aborted on its first frame).
    fn assert_empty_extra_dim_grid_has_the_initial_extent(swmr: bool) {
        let path = temp_path(&format!("hdf5_empty_grid_{swmr}"));
        let mut writer = Hdf5Writer::new();
        writer.set_swmr_mode(swmr);
        writer.set_n_extra_dims(1);
        writer.set_extra_dim_size(0, 2);
        writer.set_extra_dim_size(1, 3);
        let arr = NDArray::new(
            vec![NDDimension::new(4), NDDimension::new(4)],
            NDDataType::UInt8,
        );
        writer
            .open_file(
                &path,
                NDFileOpenMode::WRITE | NDFileOpenMode::MULTIPLE,
                &arr,
            )
            .unwrap();
        writer.close_file().unwrap();
        let h5 = H5File::open(&path).unwrap();
        let ds = h5.dataset("entry/instrument/detector/data").unwrap();
        assert_eq!(ds.shape(), vec![1, 1, 4, 4], "swmr={swmr}");
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn empty_extra_dim_grid_has_the_initial_extent() {
        assert_empty_extra_dim_grid_has_the_initial_extent(false);
    }

    #[test]
    fn swmr_empty_extra_dim_grid_has_the_initial_extent() {
        assert_empty_extra_dim_grid_has_the_initial_extent(true);
    }

    #[test]
    fn zero_frame_file_holds_the_layout() {
        assert_zero_frame_file_holds_the_layout(false);
    }

    #[test]
    fn swmr_zero_frame_file_holds_the_layout() {
        assert_zero_frame_file_holds_the_layout(true);
    }

    /// C's default NDAttribute group is the `ndattr_default` group, else the
    /// file root, else none under `auto_ndattr_default="false"`
    /// (NDFileHDF5.cpp:2766-2782, :2829); `timestamp` goes there too unless
    /// the layout puts it in a group named `performance` (:2603-2632).
    fn assert_default_groups_follow_c(swmr: bool) {
        let has = |path: &Path, ds: &str| H5File::open(path).unwrap().dataset(ds).is_ok();
        let root = write_through_layout(
            swmr,
            "root_home",
            r#"<hdf5_layout><group name="entry">
              <dataset name="data" source="detector" det_default="true"/>
            </group></hdf5_layout>"#,
        );
        assert!(has(&root, "exposure"), "swmr={swmr}");
        assert!(!has(&root, "NDAttributes/exposure"), "swmr={swmr}");
        assert!(has(&root, "timestamp"), "swmr={swmr}");
        assert!(!has(&root, "performance/timestamp"), "swmr={swmr}");
        std::fs::remove_file(&root).ok();

        let none = write_through_layout(
            swmr,
            "no_home",
            r#"<hdf5_layout auto_ndattr_default="false"><group name="entry">
              <dataset name="data" source="detector" det_default="true"/>
            </group></hdf5_layout>"#,
        );
        assert!(has(&none, "entry/data"), "swmr={swmr}");
        for ds in [
            "exposure",
            "NDAttributes/exposure",
            "timestamp",
            "performance/timestamp",
        ] {
            assert!(!has(&none, ds), "swmr={swmr} {ds}");
        }
        std::fs::remove_file(&none).ok();

        for (group, want) in [
            ("perf", "entry/NDAttributes/timestamp"),
            ("performance", "entry/performance/timestamp"),
        ] {
            let file = write_through_layout(
                swmr,
                group,
                &format!(
                    r#"<hdf5_layout><group name="entry">
                      <dataset name="data" source="detector" det_default="true"/>
                      <group name="NDAttributes" ndattr_default="true"/>
                      <group name="{group}"><dataset name="timestamp"/></group>
                    </group></hdf5_layout>"#
                ),
            );
            assert!(has(&file, "entry/NDAttributes/exposure"), "swmr={swmr}");
            assert!(has(&file, want), "swmr={swmr} {want}");
            assert!(!has(&file, "entry/perf/timestamp"), "swmr={swmr} {group}");
            std::fs::remove_file(&file).ok();
        }
    }

    /// C's layout tree keeps each group's children in `std::map`s, so every
    /// first-match lookup (`find_dset_ndattr`, `find_detector_default_dset`,
    /// `find_ndattr_default_group`; NDFileHDF5Layout.cpp:430-520) takes them in
    /// name order, not document order.
    fn assert_layout_lookups_take_name_order(swmr: bool) {
        let file = write_through_layout(
            swmr,
            "name_order",
            r#"<hdf5_layout><group name="entry">
              <group name="zeta"><dataset name="acq" source="ndattribute" ndattribute="exposure"/></group>
              <group name="alpha"><dataset name="acq" source="ndattribute" ndattribute="exposure"/></group>
              <dataset name="zdata" source="detector" det_default="true"/>
              <dataset name="adata" source="detector" det_default="true"/>
              <group name="zz" ndattr_default="true"/>
              <group name="aa" ndattr_default="true"/>
            </group></hdf5_layout>"#,
        );
        let h5 = H5File::open(&file).unwrap();
        assert!(h5.dataset("entry/alpha/acq").is_ok(), "swmr={swmr}");
        assert!(h5.dataset("entry/zeta/acq").is_err(), "swmr={swmr}");
        assert!(h5.dataset("entry/aa/timestamp").is_ok(), "swmr={swmr}");
        assert!(h5.dataset("entry/zz/timestamp").is_err(), "swmr={swmr}");
        assert_eq!(
            h5.dataset("entry/adata").unwrap().shape()[0],
            2,
            "swmr={swmr}"
        );
        std::fs::remove_file(&file).ok();
    }

    /// C refuses a group or dataset whose name its group already holds
    /// (`Group::name_exist`, NDFileHDF5Layout.cpp:329-383, :615-625): the
    /// first wins and the file is written.
    fn assert_layout_duplicate_names_keep_first(swmr: bool) {
        let file = write_through_layout(
            swmr,
            "dup_names",
            r#"<hdf5_layout><group name="entry">
              <dataset name="data" source="detector" det_default="true"/>
              <group name="g"><dataset name="first" source="constant" value="1" type="int"/></group>
              <group name="g"><dataset name="second" source="constant" value="2" type="int"/></group>
              <group name="data"><attribute name="x" source="constant" value="1" type="int"/></group>
              <group name="NDAttributes" ndattr_default="true"/>
            </group></hdf5_layout>"#,
        );
        let h5 = H5File::open(&file).unwrap();
        assert_eq!(
            h5.dataset("entry/data").unwrap().shape(),
            vec![2, 4],
            "swmr={swmr}"
        );
        assert!(h5.dataset("entry/g/first").is_ok(), "swmr={swmr}");
        assert!(h5.dataset("entry/g/second").is_err(), "swmr={swmr}");
        assert!(
            h5.dataset("entry/NDAttributes/timestamp").is_ok(),
            "swmr={swmr}"
        );
        std::fs::remove_file(&file).ok();
    }

    #[test]
    fn layout_duplicate_names_keep_first() {
        assert_layout_duplicate_names_keep_first(false);
    }

    #[test]
    fn swmr_layout_duplicate_names_keep_first() {
        assert_layout_duplicate_names_keep_first(true);
    }

    #[test]
    fn layout_lookups_take_name_order() {
        assert_layout_lookups_take_name_order(false);
    }

    #[test]
    fn swmr_layout_lookups_take_name_order() {
        assert_layout_lookups_take_name_order(true);
    }

    #[test]
    fn default_groups_follow_c() {
        assert_default_groups_follow_c(false);
    }

    #[test]
    fn swmr_default_groups_follow_c() {
        assert_default_groups_follow_c(true);
    }

    /// ADP-102. C's per-frame attribute record comes out of `pFileAttributes`,
    /// which `openFile` clears (NDFileHDF5.cpp:243) and every `writeFile`
    /// merges into (`:1437`) through `NDAttributeList::copy`
    /// (NDAttributeList.cpp:185-205) — a copy that adds and overwrites but
    /// never removes. An attribute that stops arriving therefore keeps its last
    /// value for the rest of the file, and C re-writes that value each frame.
    /// Reading the frame's own list instead turned a drop-out into a 0.
    #[test]
    fn attribute_that_drops_out_keeps_its_last_value() {
        let path = temp_path("hdf5_attr_sticky");
        let mut writer = Hdf5Writer::new();
        writer.set_num_capture(0);

        let mk = |exposure: Option<f64>| {
            let mut arr = NDArray::new(vec![NDDimension::new(4)], NDDataType::UInt8);
            arr.attributes.add(NDAttribute::new_static(
                "count",
                "",
                NDAttrSource::Driver,
                NDAttrValue::Int32(7),
            ));
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

        let a0 = mk(Some(0.5));
        writer
            .open_file(&path, NDFileOpenMode::WRITE | NDFileOpenMode::MULTIPLE, &a0)
            .unwrap();
        writer.write_file(&Arc::new(a0)).unwrap();
        // Present and changed: the new value wins.
        writer.write_file(&Arc::new(mk(Some(0.75)))).unwrap();
        // Absent: the previous value stands, it does not become 0.
        writer.write_file(&Arc::new(mk(None))).unwrap();
        writer.close_file().unwrap();

        let h5 = H5File::open(&path).unwrap();
        let exp = h5
            .dataset("entry/instrument/NDAttributes/exposure")
            .unwrap();
        assert_eq!(exp.shape(), vec![3]);
        let exp_vals: Vec<f64> = exp.read_raw().unwrap();
        assert_eq!(exp_vals, vec![0.5, 0.75, 0.75]);

        std::fs::remove_file(&path).ok();
    }

    /// ADP-102, the other side: an attribute first seen after open joins the
    /// sticky list but has no dataset, because C builds `attrList` once from
    /// `pFileAttributes` at open (`createAttributeDataset`,
    /// NDFileHDF5.cpp:2782-2857) and never revisits it.
    #[test]
    fn attribute_appearing_after_open_creates_no_dataset() {
        let path = temp_path("hdf5_attr_late");
        let mut writer = Hdf5Writer::new();
        writer.set_num_capture(0);

        let mut a0 = NDArray::new(vec![NDDimension::new(4)], NDDataType::UInt8);
        a0.attributes.add(NDAttribute::new_static(
            "count",
            "",
            NDAttrSource::Driver,
            NDAttrValue::Int32(7),
        ));
        let mut a1 = a0.clone();
        a1.attributes.add(NDAttribute::new_static(
            "late",
            "",
            NDAttrSource::Driver,
            NDAttrValue::Int32(1),
        ));

        writer
            .open_file(&path, NDFileOpenMode::WRITE | NDFileOpenMode::MULTIPLE, &a0)
            .unwrap();
        writer.write_file(&Arc::new(a0)).unwrap();
        writer.write_file(&Arc::new(a1)).unwrap();
        writer.close_file().unwrap();

        let h5 = H5File::open(&path).unwrap();
        assert_eq!(
            h5.dataset("entry/instrument/NDAttributes/count")
                .unwrap()
                .shape(),
            vec![2]
        );
        assert!(
            h5.dataset("entry/instrument/NDAttributes/late").is_err(),
            "an attribute absent at open gets no dataset"
        );

        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn test_adp78_undefined_attribute_dataset_is_float32_nan() {
        assert_undefined_attribute_dataset_is_float32_nan(false);
    }

    #[test]
    fn swmr_undefined_attribute_dataset_is_float32_nan() {
        assert_undefined_attribute_dataset_is_float32_nan(true);
    }

    fn assert_undefined_attribute_dataset_is_float32_nan(swmr: bool) {
        // ADP-78. C fixes an attribute dataset's HDF5 datatype and fill value
        // once, at file-open, from `ndAttr->getDataType()` (NDFileHDF5.cpp:2797,
        // :2830 → `typeAsHdf`, NDFileHDF5AttributeDataset.cpp:73-97). An
        // attribute with no value takes the `default:` arm — `H5T_NATIVE_FLOAT`
        // with an `epicsNAN` fill (`:366-370`) — and every element write is
        // gated on `!isUndefined_` (`:160`), so the NaN fill stands for every
        // frame even after a concrete value shows up. The port used to take
        // `NDAttrValue::Undefined.data_type()`, which is `Int32`, and wrote a
        // measured-looking 0 for the frames that carried nothing.
        let path = temp_path("hdf5_attr_undef");
        let mut writer = Hdf5Writer::new();
        writer.set_swmr_mode(swmr);
        writer.set_num_capture(0);

        let mk = |undef_value: NDAttrValue| {
            let mut arr = NDArray::new(vec![NDDimension::new(4)], NDDataType::UInt8);
            arr.attributes.add(NDAttribute::new_static(
                "novalue",
                "",
                NDAttrSource::Driver,
                undef_value,
            ));
            arr
        };

        // The frame that opens the file carries no value for the attribute.
        let a0 = mk(NDAttrValue::Undefined);
        writer
            .open_file(&path, NDFileOpenMode::WRITE | NDFileOpenMode::MULTIPLE, &a0)
            .unwrap();
        writer.write_file(&Arc::new(a0)).unwrap();
        // A concrete value arriving later must not retype the dataset, and must
        // not land in it: C never re-runs `typeAsHdf` and never lifts the gate.
        writer
            .write_file(&Arc::new(mk(NDAttrValue::Int32(7))))
            .unwrap();
        writer
            .write_file(&Arc::new(mk(NDAttrValue::Int32(9))))
            .unwrap();
        writer.close_file().unwrap();

        let h5 = H5File::open(&path).unwrap();
        let ds = h5.dataset("entry/instrument/NDAttributes/novalue").unwrap();
        assert_eq!(ds.shape(), vec![3]);
        // Float32, not the Int32 the first concrete value would imply.
        let vals: Vec<f32> = ds.read_raw().unwrap();
        assert_eq!(vals.len(), 3);
        for (i, v) in vals.iter().enumerate() {
            assert!(
                v.is_nan(),
                "element {} is {} — a reader cannot tell it from a measured value; C leaves epicsNAN",
                i,
                v
            );
        }

        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn test_attribute_dataset_chunk_matches_capture_target() {
        assert_attribute_dataset_chunk_matches_capture_target(false);
    }

    #[test]
    fn swmr_attribute_dataset_chunk_matches_capture_target() {
        assert_attribute_dataset_chunk_matches_capture_target(true);
    }

    fn assert_attribute_dataset_chunk_matches_capture_target(swmr: bool) {
        // C `calculateAttributeChunking` (NDFileHDF5.cpp:2869-2920): with the
        // default auto chunk param (0) in a non-Single mode, the attribute
        // dataset chunks at NDFileNumCapture, NOT the actual frame count — the
        // dataset is extensible so the chunk may exceed the current extent.
        // Capture target 10, only 3 frames written: chunk dim 0 must be 10,
        // extent 3; values still round-trip through the single padded chunk.
        let path = temp_path("hdf5_attr_chunk_cap");
        let mut writer = Hdf5Writer::new();
        writer.set_swmr_mode(swmr);
        writer.set_num_capture(10);

        let mk = |c: i32| {
            let mut arr = NDArray::new(vec![NDDimension::new(4)], NDDataType::UInt8);
            arr.attributes.add(NDAttribute::new_static(
                "count",
                "",
                NDAttrSource::Driver,
                NDAttrValue::Int32(c),
            ));
            arr
        };

        let a0 = mk(1);
        writer
            .open_file(&path, NDFileOpenMode::WRITE | NDFileOpenMode::MULTIPLE, &a0)
            .unwrap();
        writer.write_file(&Arc::new(a0)).unwrap();
        writer.write_file(&Arc::new(mk(2))).unwrap();
        writer.write_file(&Arc::new(mk(3))).unwrap();
        writer.close_file().unwrap();

        let h5 = H5File::open(&path).unwrap();
        let ds = h5.dataset("entry/instrument/NDAttributes/count").unwrap();
        assert_eq!(ds.shape(), vec![3]);
        assert_eq!(ds.chunk_dims(), Some(vec![10]));
        let vals: Vec<i32> = ds.read_raw().unwrap();
        assert_eq!(vals, vec![1, 2, 3]);
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn test_attribute_dataset_chunk_single_mode_is_one() {
        // Auto chunk param (0) in Single mode resolves to 1
        // (`calculateAttributeChunking`: fileWriteMode == NDFileModeSingle → 1),
        // regardless of the capture target.
        let path = temp_path("hdf5_attr_chunk_single");
        let mut writer = Hdf5Writer::new();
        writer.set_num_capture(64);

        let mut arr = NDArray::new(vec![NDDimension::new(4)], NDDataType::UInt8);
        arr.attributes.add(NDAttribute::new_static(
            "count",
            "",
            NDAttrSource::Driver,
            NDAttrValue::Int32(7),
        ));
        writer
            .open_file(&path, NDFileOpenMode::WRITE, &arr)
            .unwrap();
        writer.write_file(&Arc::new(arr)).unwrap();
        writer.close_file().unwrap();

        let h5 = H5File::open(&path).unwrap();
        let ds = h5.dataset("entry/instrument/NDAttributes/count").unwrap();
        assert_eq!(ds.chunk_dims(), Some(vec![1]));
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn test_string_attribute_dataset_is_rank1_fixed_string() {
        assert_string_attribute_dataset_is_rank1_fixed_string(false);
    }

    #[test]
    fn swmr_string_attribute_dataset_is_rank1_fixed_string() {
        assert_string_attribute_dataset_is_rank1_fixed_string(true);
    }

    fn assert_string_attribute_dataset_is_rank1_fixed_string(swmr: bool) {
        // C stores a string-valued NDAttribute as a rank-1 `[n]` dataset of a
        // fixed 256-byte H5T_C_S1 string (NDFileHDF5AttributeDataset.cpp:321-323,
        // rank_=1), NOT a 2-D `[n,256]` uint8 array. Verify rank 1, a 256-byte
        // string element, and that the null-terminated bytes round-trip.
        let path = temp_path("hdf5_attr_str");
        let mut writer = Hdf5Writer::new();
        writer.set_swmr_mode(swmr);
        writer.set_num_capture(0);

        let mk = |mode_name: &str| {
            let mut arr = NDArray::new(vec![NDDimension::new(4)], NDDataType::UInt8);
            arr.attributes.add(NDAttribute::new_static(
                "ColorMode",
                "",
                NDAttrSource::Driver,
                NDAttrValue::String(mode_name.to_string()),
            ));
            arr
        };

        let a0 = mk("Mono");
        writer
            .open_file(&path, NDFileOpenMode::WRITE | NDFileOpenMode::MULTIPLE, &a0)
            .unwrap();
        writer.write_file(&Arc::new(a0)).unwrap();
        writer.write_file(&Arc::new(mk("RGB1"))).unwrap();
        writer.close_file().unwrap();

        let h5 = H5File::open(&path).unwrap();
        // The default layout declares ColorMode as a `<dataset source="ndattribute">`
        // in the detector subgroup, so it is routed there, not the default group.
        let ds = h5
            .dataset("entry/instrument/detector/NDAttributes/ColorMode")
            .unwrap();
        // Rank-1 [2] of a 256-byte fixed-length string element, not [2,256] u8.
        assert_eq!(ds.shape(), vec![2]);
        assert_eq!(ds.element_size(), MAX_ATTRIBUTE_STRING_SIZE);

        let frames: Vec<FixedStr256> = ds.read_raw().unwrap();
        assert_eq!(frames.len(), 2);
        let decode = |f: &FixedStr256| -> String {
            f.0.iter()
                .take_while(|&&b| b != 0)
                .map(|&b| b as char)
                .collect()
        };
        assert_eq!(decode(&frames[0]), "Mono");
        assert_eq!(decode(&frames[1]), "RGB1");
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn test_ndattribute_dataset_routed_to_declared_group() {
        // C `find_dset_ndattr` (NDFileHDF5.cpp:2792) places an NDAttribute that
        // matches a `<dataset source="ndattribute">` declaration into that
        // dataset's parent group; the default layout declares ColorMode in the
        // detector subgroup. An NDAttribute with no declaration falls back to
        // the `ndattr_default` group. Verify both in a single file.
        let path = temp_path("hdf5_attr_route");
        let mut writer = Hdf5Writer::new();

        let mut arr = NDArray::new(vec![NDDimension::new(4)], NDDataType::UInt8);
        arr.attributes.add(NDAttribute::new_static(
            "ColorMode",
            "",
            NDAttrSource::Driver,
            NDAttrValue::String("Mono".to_string()),
        ));
        arr.attributes.add(NDAttribute::new_static(
            "count",
            "",
            NDAttrSource::Driver,
            NDAttrValue::Int32(7),
        ));

        writer
            .open_file(
                &path,
                NDFileOpenMode::WRITE | NDFileOpenMode::MULTIPLE,
                &arr,
            )
            .unwrap();
        writer.write_file(&Arc::new(arr)).unwrap();
        writer.close_file().unwrap();

        let h5 = H5File::open(&path).unwrap();
        // Declared → routed into the detector subgroup.
        assert!(
            h5.dataset("entry/instrument/detector/NDAttributes/ColorMode")
                .is_ok()
        );
        // Undeclared → default ndattr group.
        assert!(h5.dataset("entry/instrument/NDAttributes/count").is_ok());
        // ColorMode must NOT also appear in the default group.
        assert!(
            h5.dataset("entry/instrument/NDAttributes/ColorMode")
                .is_err()
        );
        std::fs::remove_file(&path).ok();
    }

    /// C `storeOnOpenCloseAttribute` (NDFileHDF5.cpp:553-632) writes a
    /// layout `<attribute source="ndattribute">` as an HDF5 attribute on its
    /// group/dataset: `when="OnFileOpen"` (and the default `OnFrame`) take
    /// the first-frame value, `when="OnFileClose"` the last. Cover a group
    /// string attribute and a dataset numeric attribute, both phases; under
    /// SWMR the close ones are written after `start_swmr`, as C's
    /// `storeOnCloseAttributes`.
    fn assert_ndattribute_element_attr_open_close_values(swmr: bool) {
        let dir = std::env::temp_dir();
        let layout = dir.join(format!(
            "adcore_layout_elem_attr_{swmr}_{}.xml",
            std::process::id()
        ));
        std::fs::write(
            &layout,
            r#"<hdf5_layout>
              <group name="entry">
                <group name="instrument">
                  <group name="detector">
                    <attribute name="FirstColorMode" source="ndattribute" ndattribute="ColorMode" when="OnFileOpen"/>
                    <attribute name="LastColorMode" source="ndattribute" ndattribute="ColorMode" when="OnFileClose"/>
                    <attribute name="DefaultColorMode" source="ndattribute" ndattribute="ColorMode"/>
                    <dataset name="data" source="detector" det_default="true">
                      <attribute name="GainAtOpen" source="ndattribute" ndattribute="Gain" when="OnFileOpen"/>
                      <attribute name="GainAtClose" source="ndattribute" ndattribute="Gain" when="OnFileClose"/>
                    </dataset>
                  </group>
                  <group name="NDAttributes" ndattr_default="true"/>
                </group>
              </group>
            </hdf5_layout>"#,
        )
        .unwrap();

        let path = temp_path(&format!("hdf5_elem_attr_{swmr}"));
        let mut writer = Hdf5Writer::new();
        writer.set_num_capture(0);
        writer.set_swmr_mode(swmr);
        writer.set_layout_filename(layout.to_str().unwrap());

        let mk = |mode: &str, gain: i32| {
            let mut arr = NDArray::new(vec![NDDimension::new(4)], NDDataType::UInt8);
            arr.attributes.add(NDAttribute::new_static(
                "ColorMode",
                "",
                NDAttrSource::Driver,
                NDAttrValue::String(mode.to_string()),
            ));
            arr.attributes.add(NDAttribute::new_static(
                "Gain",
                "",
                NDAttrSource::Driver,
                NDAttrValue::Int32(gain),
            ));
            arr
        };

        let a0 = mk("Mono", 10);
        writer
            .open_file(&path, NDFileOpenMode::WRITE | NDFileOpenMode::MULTIPLE, &a0)
            .unwrap();
        writer.write_file(&Arc::new(a0)).unwrap();
        writer.write_file(&Arc::new(mk("RGB1", 20))).unwrap();
        writer.write_file(&Arc::new(mk("Bayer", 30))).unwrap();
        writer.close_file().unwrap();

        let h5 = H5File::open(&path).unwrap();

        // Group string element-attributes on /entry/instrument/detector.
        let mut grp = h5.root_group();
        for seg in ["entry", "instrument", "detector"] {
            grp = grp.group(seg).unwrap();
        }
        // OnFileOpen and the default (OnFrame, open wins) take the first value.
        assert_eq!(grp.attr_string("FirstColorMode").unwrap(), "Mono");
        assert_eq!(grp.attr_string("DefaultColorMode").unwrap(), "Mono");
        // OnFileClose takes the last value.
        assert_eq!(
            grp.attr_string("LastColorMode").unwrap(),
            "Bayer",
            "swmr={swmr}"
        );

        // Dataset numeric element-attributes on the detector dataset.
        let ds = h5.dataset("entry/instrument/detector/data").unwrap();
        assert_eq!(
            ds.attr("GainAtOpen")
                .unwrap()
                .read_numeric::<i32>()
                .unwrap(),
            10
        );
        assert_eq!(
            ds.attr("GainAtClose")
                .unwrap()
                .read_numeric::<i32>()
                .unwrap(),
            30
        );

        std::fs::remove_file(&path).ok();
        std::fs::remove_file(&layout).ok();
    }

    #[test]
    fn test_ndattribute_element_attr_open_close_values() {
        assert_ndattribute_element_attr_open_close_values(false);
    }

    #[test]
    fn swmr_ndattribute_element_attr_open_close_values() {
        assert_ndattribute_element_attr_open_close_values(true);
    }

    #[test]
    fn test_ndattr_element_attr_on_lazily_created_dataset() {
        // C storeOnOpenCloseAttribute attaches a layout `<attribute
        // source="ndattribute">` to ANY element the layout names, not just the
        // detector dataset. A lazily-created NDAttribute dataset has no retained
        // create-time handle, so its element-attrs are attached by reopening the
        // dataset by name in write mode (rust-hdf5 0.2.22 dataset_writer). Here
        // a `GainTrace` NDAttribute dataset (sourced from Gain) carries two
        // element-attrs sourced from ColorMode (open=first, close=last value).
        let dir = std::env::temp_dir();
        let layout = dir.join(format!(
            "adcore_layout_lazy_ds_attr_{}.xml",
            std::process::id()
        ));
        std::fs::write(
            &layout,
            r#"<hdf5_layout>
              <group name="entry">
                <group name="instrument">
                  <group name="detector">
                    <dataset name="data" source="detector" det_default="true"/>
                    <dataset name="GainTrace" source="ndattribute" ndattribute="Gain">
                      <attribute name="ModeAtOpen" source="ndattribute" ndattribute="ColorMode" when="OnFileOpen"/>
                      <attribute name="ModeAtClose" source="ndattribute" ndattribute="ColorMode" when="OnFileClose"/>
                    </dataset>
                  </group>
                  <group name="NDAttributes" ndattr_default="true"/>
                </group>
              </group>
            </hdf5_layout>"#,
        )
        .unwrap();

        let path = temp_path("hdf5_lazy_ds_attr");
        let mut writer = Hdf5Writer::new();
        writer.set_num_capture(0);
        writer.set_layout_filename(layout.to_str().unwrap());

        let mk = |mode: &str, gain: i32| {
            let mut arr = NDArray::new(vec![NDDimension::new(4)], NDDataType::UInt8);
            arr.attributes.add(NDAttribute::new_static(
                "ColorMode",
                "",
                NDAttrSource::Driver,
                NDAttrValue::String(mode.to_string()),
            ));
            arr.attributes.add(NDAttribute::new_static(
                "Gain",
                "",
                NDAttrSource::Driver,
                NDAttrValue::Int32(gain),
            ));
            arr
        };

        let a0 = mk("Mono", 10);
        writer
            .open_file(&path, NDFileOpenMode::WRITE | NDFileOpenMode::MULTIPLE, &a0)
            .unwrap();
        writer.write_file(&Arc::new(a0)).unwrap();
        writer.write_file(&Arc::new(mk("RGB1", 20))).unwrap();
        writer.write_file(&Arc::new(mk("Bayer", 30))).unwrap();
        writer.close_file().unwrap();

        let h5 = H5File::open(&path).unwrap();
        // The element-attrs are attached to the lazily-created NDAttribute
        // dataset (not a detector dataset, so reopened by name at close).
        let ds = h5.dataset("entry/instrument/detector/GainTrace").unwrap();
        assert_eq!(
            ds.attr("ModeAtOpen").unwrap().read_string().unwrap(),
            "Mono"
        );
        assert_eq!(
            ds.attr("ModeAtClose").unwrap().read_string().unwrap(),
            "Bayer"
        );
        std::fs::remove_file(&path).ok();
        std::fs::remove_file(&layout).ok();
    }

    #[test]
    fn a_string_ndattribute_dataset_takes_the_layout_dataset_name() {
        let layout = std::env::temp_dir().join(format!(
            "adcore_layout_string_ds_name_{}.xml",
            std::process::id()
        ));
        std::fs::write(
            &layout,
            r#"<hdf5_layout>
              <group name="entry">
                <group name="detector">
                  <dataset name="data" source="detector" det_default="true"/>
                  <dataset name="ModeTrace" source="ndattribute" ndattribute="ColorMode"/>
                </group>
              </group>
            </hdf5_layout>"#,
        )
        .unwrap();

        let path = temp_path("hdf5_string_ds_name");
        let mut writer = Hdf5Writer::new();
        writer.set_num_capture(0);
        writer.set_layout_filename(layout.to_str().unwrap());
        let mk = |mode: &str| {
            let mut arr = NDArray::new(vec![NDDimension::new(4)], NDDataType::UInt8);
            arr.attributes.add(NDAttribute::new_static(
                "ColorMode",
                "",
                NDAttrSource::Driver,
                NDAttrValue::String(mode.to_string()),
            ));
            arr
        };
        let a0 = mk("Mono");
        writer
            .open_file(&path, NDFileOpenMode::WRITE | NDFileOpenMode::MULTIPLE, &a0)
            .unwrap();
        writer.write_file(&Arc::new(a0)).unwrap();
        writer.write_file(&Arc::new(mk("RGB1"))).unwrap();
        writer.close_file().unwrap();

        let h5 = H5File::open(&path).unwrap();
        let ds = h5.dataset("entry/detector/ModeTrace").unwrap();
        assert_eq!(ds.shape(), vec![2]);
        assert!(h5.dataset("entry/detector/ColorMode").is_err());
        std::fs::remove_file(&path).ok();
        std::fs::remove_file(&layout).ok();
    }

    #[test]
    fn test_swmr_ndattr_element_attr_on_streaming_dataset() {
        // In SWMR mode an open-time `<attribute source="ndattribute">` on the
        // streaming dataset is attached before the SWMR lock via
        // set_dataset_attr_* (rust-hdf5 0.2.22 addresses SWMR dataset attributes
        // by index).
        let dir = std::env::temp_dir();
        let layout = dir.join(format!(
            "adcore_layout_swmr_ds_attr_{}.xml",
            std::process::id()
        ));
        std::fs::write(
            &layout,
            r#"<hdf5_layout>
              <group name="entry">
                <group name="instrument">
                  <group name="detector">
                    <dataset name="data" source="detector" det_default="true">
                      <attribute name="ModeAtOpen" source="ndattribute" ndattribute="ColorMode" when="OnFileOpen"/>
                    </dataset>
                  </group>
                  <group name="NDAttributes" ndattr_default="true"/>
                </group>
              </group>
            </hdf5_layout>"#,
        )
        .unwrap();

        let path = temp_path("hdf5_swmr_ds_attr");
        let mut writer = Hdf5Writer::new();
        writer.set_swmr_mode(true);
        writer.set_num_capture(0);
        writer.set_layout_filename(layout.to_str().unwrap());

        let mk = |mode: &str| {
            let mut arr = NDArray::new(
                vec![NDDimension::new(4), NDDimension::new(4)],
                NDDataType::UInt16,
            );
            arr.attributes.add(NDAttribute::new_static(
                "ColorMode",
                "",
                NDAttrSource::Driver,
                NDAttrValue::String(mode.to_string()),
            ));
            arr
        };

        let a0 = mk("Mono");
        writer
            .open_file(&path, NDFileOpenMode::WRITE | NDFileOpenMode::MULTIPLE, &a0)
            .unwrap();
        writer.write_file(&Arc::new(a0)).unwrap();
        writer.write_file(&Arc::new(mk("RGB1"))).unwrap();
        writer.close_file().unwrap();

        let h5 = H5File::open(&path).unwrap();
        let ds = h5.dataset("entry/instrument/detector/data").unwrap();
        assert_eq!(
            ds.attr("ModeAtOpen").unwrap().read_string().unwrap(),
            "Mono"
        );
        std::fs::remove_file(&path).ok();
        std::fs::remove_file(&layout).ok();
    }

    #[test]
    fn test_swmr_ndarray_default_attrs_on_streaming_dataset() {
        // C writeDefaultDatasetAttributes (NDFileHDF5.cpp:3695-3719) attaches
        // NDArrayNumDims plus the per-dimension NDArrayDimOffset/Binning/Reverse
        // to every detector dataset. In SWMR mode they are written on the single
        // streaming dataset before start_swmr() locks the file, addressed by
        // dataset index (rust-hdf5 0.2.22). writeH5attrInt32
        // (NDFileHDF5.cpp:1142-1191): 1-D array => scalar int32, multi-dim => a
        // 1-D int32 array of length ndims, native dim order.

        // 1-D streaming array: all four attributes present and scalar.
        let path = temp_path("hdf5_swmr_dimattr_1d");
        let mut writer = Hdf5Writer::new();
        writer.set_swmr_mode(true);
        writer.set_num_capture(0);
        let mut d = NDDimension::new(8);
        d.offset = 3;
        d.binning = 2;
        d.reverse = true;
        let arr = NDArray::new(vec![d], NDDataType::UInt16);
        writer
            .open_file(
                &path,
                NDFileOpenMode::WRITE | NDFileOpenMode::MULTIPLE,
                &arr,
            )
            .unwrap();
        writer.write_file(&Arc::new(arr.clone())).unwrap();
        writer.write_file(&Arc::new(arr)).unwrap();
        writer.close_file().unwrap();
        let h5 = H5File::open(&path).unwrap();
        let ds = h5.dataset("entry/instrument/detector/data").unwrap();
        let geti = |n: &str| -> i32 { ds.attr(n).unwrap().read_numeric().unwrap() };
        assert_eq!(geti("NDArrayNumDims"), 1);
        assert_eq!(geti("NDArrayDimOffset"), 3);
        assert_eq!(geti("NDArrayDimBinning"), 2);
        assert_eq!(geti("NDArrayDimReverse"), 1);
        std::fs::remove_file(&path).ok();

        // 2-D streaming array: NDArrayNumDims=2; the Dim* attributes are 1-D
        // int32 arrays of length 2 in native dim order (dims[0] then dims[1]).
        let path = temp_path("hdf5_swmr_dimattr_2d");
        let mut writer = Hdf5Writer::new();
        writer.set_swmr_mode(true);
        writer.set_num_capture(0);
        let mut d0 = NDDimension::new(4);
        d0.offset = 3;
        d0.binning = 2;
        d0.reverse = true;
        let mut d1 = NDDimension::new(8);
        d1.offset = 5;
        d1.binning = 4;
        d1.reverse = false;
        let arr = NDArray::new(vec![d0, d1], NDDataType::UInt16);
        writer
            .open_file(
                &path,
                NDFileOpenMode::WRITE | NDFileOpenMode::MULTIPLE,
                &arr,
            )
            .unwrap();
        writer.write_file(&Arc::new(arr.clone())).unwrap();
        writer.write_file(&Arc::new(arr)).unwrap();
        writer.close_file().unwrap();
        let h5 = H5File::open(&path).unwrap();
        let ds = h5.dataset("entry/instrument/detector/data").unwrap();
        let numdims: i32 = ds.attr("NDArrayNumDims").unwrap().read_numeric().unwrap();
        assert_eq!(numdims, 2);
        let read_i32_arr = |n: &str| -> Vec<i32> {
            let raw = ds.attr(n).unwrap().read_raw().unwrap();
            assert_eq!(raw.len(), 2 * 4, "{n} must be a 2-element int32 array");
            raw.as_chunks::<4>()
                .0
                .iter()
                .map(|b| i32::from_le_bytes([b[0], b[1], b[2], b[3]]))
                .collect()
        };
        assert_eq!(read_i32_arr("NDArrayDimOffset"), vec![3, 5]);
        assert_eq!(read_i32_arr("NDArrayDimBinning"), vec![2, 4]);
        assert_eq!(read_i32_arr("NDArrayDimReverse"), vec![1, 0]);
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn test_ndattr_descriptor_attributes_written() {
        // C attaches NDAttrName/NDAttrDescription/NDAttrSourceType/NDAttrSource
        // string HDF5 attributes (non-empty only) to every NDAttribute dataset
        // (NDFileHDF5.cpp:2715, 2817-2822).
        let path = temp_path("hdf5_attr_desc");
        let mut writer = Hdf5Writer::new();
        writer.set_num_capture(0);

        let mk = |v: f64| {
            let mut arr = NDArray::new(vec![NDDimension::new(4)], NDDataType::UInt8);
            arr.attributes.add(NDAttribute::new_static(
                "AcquireTime",
                "exposure time",
                NDAttrSource::EpicsPV("13SIM1:cam1:AcquireTime_RBV".to_string()),
                NDAttrValue::Float64(v),
            ));
            arr
        };

        let a0 = mk(0.1);
        writer
            .open_file(&path, NDFileOpenMode::WRITE | NDFileOpenMode::MULTIPLE, &a0)
            .unwrap();
        writer.write_file(&Arc::new(a0)).unwrap();
        writer.write_file(&Arc::new(mk(0.2))).unwrap();
        writer.close_file().unwrap();

        let h5 = H5File::open(&path).unwrap();
        let ds = h5
            .dataset("entry/instrument/NDAttributes/AcquireTime")
            .unwrap();
        let read = |n: &str| ds.attr(n).unwrap().read_string().unwrap();
        assert_eq!(read("NDAttrName"), "AcquireTime");
        assert_eq!(read("NDAttrDescription"), "exposure time");
        assert_eq!(read("NDAttrSourceType"), "NDAttrSourceEPICSPV");
        assert_eq!(read("NDAttrSource"), "13SIM1:cam1:AcquireTime_RBV");
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn test_detector_dataset_ndarray_dim_attributes() {
        // C writeDefaultDatasetAttributes (NDFileHDF5.cpp:3695-3719) attaches
        // NDArrayNumDims (scalar) and the per-dimension NDArrayDimOffset/Binning/
        // Reverse. writeH5attrInt32 (NDFileHDF5.cpp:1142-1191) emits a single
        // dimension as a scalar int32 and multiple dimensions as a 1-D int32
        // array of length ndims, in native dim order.

        // 1-D array: all four attributes present and scalar, in native order.
        let path = temp_path("hdf5_dimattr_1d");
        let mut writer = Hdf5Writer::new();
        let mut d = NDDimension::new(8);
        d.offset = 3;
        d.binning = 2;
        d.reverse = true;
        let arr = NDArray::new(vec![d], NDDataType::UInt16);
        writer
            .open_file(&path, NDFileOpenMode::WRITE, &arr)
            .unwrap();
        writer.write_file(&Arc::new(arr)).unwrap();
        writer.close_file().unwrap();
        let h5 = H5File::open(&path).unwrap();
        let ds = h5.dataset("entry/instrument/detector/data").unwrap();
        let geti = |n: &str| -> i32 { ds.attr(n).unwrap().read_numeric().unwrap() };
        assert_eq!(geti("NDArrayNumDims"), 1);
        assert_eq!(geti("NDArrayDimOffset"), 3);
        assert_eq!(geti("NDArrayDimBinning"), 2);
        assert_eq!(geti("NDArrayDimReverse"), 1);
        std::fs::remove_file(&path).ok();

        // 2-D array: NDArrayNumDims present (=2); the Dim* attributes are 1-D
        // int32 arrays of length 2, in native dim order. Distinct per-dim values
        // verify the order is dims[0] then dims[1] (not reversed HDF5 axes).
        let path = temp_path("hdf5_dimattr_2d");
        let mut writer = Hdf5Writer::new();
        let mut d0 = NDDimension::new(4);
        d0.offset = 3;
        d0.binning = 2;
        d0.reverse = true;
        let mut d1 = NDDimension::new(8);
        d1.offset = 5;
        d1.binning = 4;
        d1.reverse = false;
        let arr = NDArray::new(vec![d0, d1], NDDataType::UInt16);
        writer
            .open_file(&path, NDFileOpenMode::WRITE, &arr)
            .unwrap();
        writer.write_file(&Arc::new(arr)).unwrap();
        writer.close_file().unwrap();
        let h5 = H5File::open(&path).unwrap();
        let ds = h5.dataset("entry/instrument/detector/data").unwrap();
        let numdims: i32 = ds.attr("NDArrayNumDims").unwrap().read_numeric().unwrap();
        assert_eq!(numdims, 2);
        // The array attribute is a 1-D simple dataspace of length ndims: read the
        // raw int32 LE bytes back (4 bytes/elem => 8 bytes proves an array, not a
        // scalar). h5py/libhdf5 reads the same shape and values.
        let read_i32_arr = |n: &str| -> Vec<i32> {
            let raw = ds.attr(n).unwrap().read_raw().unwrap();
            assert_eq!(raw.len(), 2 * 4, "{n} must be a 2-element int32 array");
            raw.as_chunks::<4>()
                .0
                .iter()
                .map(|b| i32::from_le_bytes([b[0], b[1], b[2], b[3]]))
                .collect()
        };
        assert_eq!(read_i32_arr("NDArrayDimOffset"), vec![3, 5]);
        assert_eq!(read_i32_arr("NDArrayDimBinning"), vec![2, 4]);
        assert_eq!(read_i32_arr("NDArrayDimReverse"), vec![1, 0]);
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn test_fill_value_recorded_on_dataset() {
        // The configured HDF5_fillValue reaches the DCPL, cast to the element
        // type as C does, and an unwritten region of a fill-valued dataset
        // reads back as `fill`.
        let path = temp_path("hdf5_fill");
        let mut writer = Hdf5Writer::new();
        writer.set_fill_value(7.5);

        let arr = NDArray::new(
            vec![NDDimension::new(4), NDDimension::new(4)],
            NDDataType::UInt16,
        );
        writer
            .open_file(&path, NDFileOpenMode::WRITE, &arr)
            .unwrap();
        writer.write_file(&Arc::new(arr)).unwrap();
        writer.close_file().unwrap();

        let h5 = H5File::open(&path).unwrap();
        let ds = h5.dataset("entry/instrument/detector/data").unwrap();
        assert_eq!(
            ds.fill_value().unwrap(),
            rust_hdf5::FillValue::UserDefined(7u16.to_le_bytes().to_vec())
        );
        std::fs::remove_file(&path).ok();

        // Direct DCPL check: a fixed-shape dataset created with fill_value and
        // never written reads back the fill value, not zero.
        let path2 = temp_path("hdf5_fill_dcpl");
        {
            let f = H5File::create(&path2).unwrap();
            let _ = f
                .new_dataset::<i32>()
                .shape(&[8][..])
                .fill_value(42i32)
                .create("unwritten")
                .unwrap();
        }
        let h5b = H5File::open(&path2).unwrap();
        let vals: Vec<i32> = h5b.dataset("unwritten").unwrap().read_raw().unwrap();
        assert_eq!(vals, vec![42i32; 8]);
        std::fs::remove_file(&path2).ok();
    }

    #[test]
    fn test_performance_dataset() {
        let path = temp_path("hdf5_perf");
        let mut writer = Hdf5Writer::new();
        writer.set_num_capture(0);
        writer.set_store_performance(true);

        let arr = NDArray::new(
            vec![NDDimension::new(8), NDDimension::new(8)],
            NDDataType::UInt16,
        );
        writer
            .open_file(
                &path,
                NDFileOpenMode::WRITE | NDFileOpenMode::MULTIPLE,
                &arr,
            )
            .unwrap();
        writer.write_file(&Arc::new(arr.clone())).unwrap();
        writer.write_file(&Arc::new(arr)).unwrap();
        writer.close_file().unwrap();

        let h5 = H5File::open(&path).unwrap();
        let ts = h5
            .dataset("entry/instrument/performance/timestamp")
            .unwrap();
        assert_eq!(ts.shape(), vec![2, 5]);
        let vals: Vec<f64> = ts.read_raw().unwrap();
        assert_eq!(vals.len(), 10);

        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn test_performance_dataset_chunk_matches_capture_target() {
        assert_performance_dataset_chunk_matches_capture_target(false);
    }

    /// createPerformanceDataset runs before startSWMR (NDFileHDF5.cpp:312-326).
    #[test]
    fn swmr_performance_dataset_chunk_matches_capture_target() {
        assert_performance_dataset_chunk_matches_capture_target(true);
    }

    fn assert_performance_dataset_chunk_matches_capture_target(swmr: bool) {
        // C `writePerformanceDataset` chunks the timestamp dataset `[chunking,5]`
        // (NDFileHDF5.cpp:2645-2647) where `chunking` is the same
        // `calculateAttributeChunking` value (the capture target in non-Single
        // mode), not one row per chunk. Capture target 8, 2 frames written: the
        // chunk's leading dim must be 8, extent 2; doubles still round-trip.
        let path = temp_path("hdf5_perf_chunk");
        let mut writer = Hdf5Writer::new();
        writer.set_swmr_mode(swmr);
        writer.set_store_performance(true);
        writer.set_num_capture(8);

        let arr = NDArray::new(
            vec![NDDimension::new(8), NDDimension::new(8)],
            NDDataType::UInt16,
        );
        writer
            .open_file(
                &path,
                NDFileOpenMode::WRITE | NDFileOpenMode::MULTIPLE,
                &arr,
            )
            .unwrap();
        writer.write_file(&Arc::new(arr.clone())).unwrap();
        writer.write_file(&Arc::new(arr)).unwrap();
        writer.close_file().unwrap();

        let h5 = H5File::open(&path).unwrap();
        let ts = h5
            .dataset("entry/instrument/performance/timestamp")
            .unwrap();
        assert_eq!(ts.shape(), vec![2, 5], "swmr={swmr}");
        assert_eq!(ts.chunk_dims(), Some(vec![8, 5]), "swmr={swmr}");
        let vals: Vec<f64> = ts.read_raw().unwrap();
        assert_eq!(vals.len(), 10);
        // Column 0 is the write time, positive on every row written.
        assert!(vals[0] > 0.0 && vals[5] > 0.0, "swmr={swmr} {vals:?}");

        std::fs::remove_file(&path).ok();
    }

    /// C `createPerformanceDataset` makes `timestamp` unlimited in both
    /// dims (`maxdims = {H5S_UNLIMITED, H5S_UNLIMITED}`, NDFileHDF5.cpp:2600);
    /// three rows over chunks of two span two chunks and trim to three.
    #[test]
    fn the_performance_dataset_is_unlimited_in_both_dims() {
        assert_performance_dataset_is_unlimited_in_both_dims(false);
    }

    /// SWMR too: C creates it before `startSWMR` and writes it at close.
    #[test]
    fn swmr_performance_dataset_is_unlimited_in_both_dims() {
        assert_performance_dataset_is_unlimited_in_both_dims(true);
    }

    fn assert_performance_dataset_is_unlimited_in_both_dims(swmr: bool) {
        let path = temp_path("hdf5_perf_maxdims");
        let mut writer = Hdf5Writer::new();
        writer.set_swmr_mode(swmr);
        writer.set_store_performance(true);
        writer.set_num_capture(4);
        let arr = Arc::new(NDArray::new(
            vec![NDDimension::new(8), NDDimension::new(8)],
            NDDataType::UInt16,
        ));
        writer
            .open_file(
                &path,
                NDFileOpenMode::WRITE | NDFileOpenMode::MULTIPLE,
                &arr,
            )
            .unwrap();
        for _ in 0..3 {
            writer.write_file(&arr).unwrap();
        }
        writer.close_file().unwrap();

        let h5 = H5File::open(&path).unwrap();
        let ts = h5
            .dataset("entry/instrument/performance/timestamp")
            .unwrap();
        assert_eq!(ts.max_shape().unwrap(), vec![None, None], "swmr={swmr}");
        assert_eq!(ts.chunk_dims(), Some(vec![4, 5]), "swmr={swmr}");
        assert_eq!(ts.shape(), vec![3, 5], "swmr={swmr}");
        let vals: Vec<f64> = ts.read_raw().unwrap();
        assert_eq!(vals.len(), 15);
        assert!(vals[10..].iter().any(|&v| v != 0.0), "swmr={swmr} {vals:?}");
        std::fs::remove_file(&path).ok();
    }

    /// C gives the detector dataset the layout's attributes (`signal` in the
    /// default layout) and writeDefaultDatasetAttributes' four, nothing else
    /// (NDFileHDF5.cpp:687, :3695-3719).
    #[test]
    fn the_detector_dataset_carries_only_c_attributes() {
        let path = temp_path("hdf5_c_attrs");
        let mut writer = Hdf5Writer::new();
        writer.set_num_capture(0);
        writer.set_n_extra_dims(1);
        writer.set_extra_dim_size(0, 2);
        writer.set_extra_dim_size(1, 2);
        writer.set_extra_dim_name(0, "n");
        let arr = Arc::new(ramp_u16(4, 5));
        let mode = NDFileOpenMode::WRITE | NDFileOpenMode::MULTIPLE;
        writer.open_file(&path, mode, &arr).unwrap();
        writer.write_file(&arr).unwrap();
        writer.close_file().unwrap();
        let h5 = H5File::open(&path).unwrap();
        let ds = h5.dataset("entry/instrument/detector/data").unwrap();
        let mut names = ds.attr_names().unwrap();
        names.sort();
        assert_eq!(
            names,
            [
                "NDArrayDimBinning",
                "NDArrayDimOffset",
                "NDArrayDimReverse",
                "NDArrayNumDims",
                "signal"
            ]
        );
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn the_detector_dataset_takes_c_dims_and_maxdims() {
        // (mode, NumCapture, nExtraDims sizes, frames) -> (shape, max, data),
        // the same in a SWMR file.
        fn layout(
            name: &str,
            mode: NDFileMode,
            num_capture: i32,
            extra: &[usize],
            frames: u16,
        ) -> (Vec<usize>, Vec<Option<usize>>, Vec<u16>) {
            let std = layout_swmr(name, mode, num_capture, extra, frames, false);
            let swmr = layout_swmr(
                &format!("{name}_swmr"),
                mode,
                num_capture,
                extra,
                frames,
                true,
            );
            assert_eq!(swmr, std, "{name}");
            std
        }
        fn layout_swmr(
            name: &str,
            mode: NDFileMode,
            num_capture: i32,
            extra: &[usize],
            frames: u16,
            swmr: bool,
        ) -> (Vec<usize>, Vec<Option<usize>>, Vec<u16>) {
            let path = temp_path(name);
            let mut writer = Hdf5Writer::new();
            writer.set_swmr_mode(swmr);
            writer.set_num_capture(num_capture);
            writer.set_n_row_chunks(3);
            writer.set_n_extra_dims(extra.len().saturating_sub(1));
            for (i, &n) in extra.iter().enumerate() {
                writer.set_extra_dim_size(i, n);
            }
            let mut arr = NDArray::new(
                vec![NDDimension::new(8), NDDimension::new(8)],
                NDDataType::UInt16,
            );
            for f in 0..frames {
                if let NDDataViewMut::U16(v) = arr.data_mut().view_mut() {
                    for (i, x) in v.iter_mut().enumerate() {
                        *x = f * 100 + i as u16;
                    }
                }
                if f == 0 {
                    writer.open_file(&path, opened(mode), &arr).unwrap();
                }
                writer.write_file(&Arc::new(arr.clone())).unwrap();
            }
            writer.close_file().unwrap();
            let h5 = H5File::open(&path).unwrap();
            let ds = h5.dataset("entry/instrument/detector/data").unwrap();
            let out = (ds.shape(), ds.max_shape().unwrap(), ds.read_raw().unwrap());
            std::fs::remove_file(&path).ok();
            out
        }
        let pixels = |frames: u16| -> Vec<u16> {
            (0..frames)
                .flat_map(|f| (0..64).map(move |i| f * 100 + i))
                .collect()
        };
        let m = |v: &[usize]| v.iter().map(|&d| Some(d)).collect::<Vec<_>>();

        // Single: C `multiFrameFile` is false, so the frame alone.
        let (shape, max, data) = layout("hdf5_c_dims_single", NDFileMode::Single, 1, &[], 1);
        assert_eq!((shape, max), (vec![8, 8], m(&[8, 8])));
        assert_eq!(data, pixels(1));
        // One frame axis, NumCapture deep or unlimited.
        let (shape, max, data) = layout("hdf5_c_dims_nc5", NDFileMode::Stream, 5, &[], 3);
        assert_eq!((shape, max), (vec![3, 8, 8], m(&[5, 8, 8])));
        assert_eq!(data, pixels(3));
        let (shape, max, _) = layout("hdf5_c_dims_nc0", NDFileMode::Stream, 0, &[], 3);
        assert_eq!((shape, max), (vec![3, 8, 8], vec![None, Some(8), Some(8)]));
        // Extra dims: C extendDataSet grows N first, then X once N wraps.
        let (shape, max, data) = layout("hdf5_c_dims_grid", NDFileMode::Stream, 0, &[2, 3], 3);
        assert_eq!((shape, max), (vec![2, 2, 8, 8], m(&[3, 2, 8, 8])));
        let mut want = pixels(3);
        want.resize(4 * 64, 0);
        assert_eq!(data, want);
    }

    #[test]
    fn test_roundtrip_all_types() {
        macro_rules! roundtrip {
            ($name:expr, $dt:expr, $variant:ident, $ty:ty, $vals:expr) => {{
                let path = temp_path($name);
                let mut writer = Hdf5Writer::new();
                let mut arr = NDArray::new(vec![NDDimension::new(4)], $dt);
                if let NDDataViewMut::$variant(v) = arr.data_mut().view_mut() {
                    let src: Vec<$ty> = $vals;
                    v.copy_from_slice(&src);
                }
                writer
                    .open_file(&path, NDFileOpenMode::WRITE, &arr)
                    .unwrap();
                writer.write_file(&Arc::new(arr)).unwrap();
                writer.close_file().unwrap();

                let h5 = H5File::open(&path).unwrap();
                let ds = h5.dataset("entry/instrument/detector/data").unwrap();
                let r = crate::test_util::read_native(&ds, None);
                assert_eq!(r.data_type(), $dt, "type for {}", $name);
                if let NDDataView::$variant(v) = r.view() {
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
        if let NDDataViewMut::U16(v) = arr.data_mut().view_mut() {
            for i in 0..v.len() {
                v[i] = (i % 256) as u16;
            }
        }

        writer
            .open_file(&path, NDFileOpenMode::WRITE, &arr)
            .unwrap();
        writer.write_file(&Arc::new(arr.clone())).unwrap();
        writer.close_file().unwrap();

        // Assert compression by COMPARING against an uncompressed baseline of
        // the same frame, not against a fixed byte threshold. Since rust-hdf5
        // 0.2.25 the NeXus group string attributes (NX_class, …) are stored as
        // variable-length UTF-8 in the global heap, so the file's constant
        // metadata overhead now dwarfs this 8 KiB frame — an absolute
        // "< 8192" check no longer reflects whether the data chunk was
        // compressed (h5dump confirms the chunk itself is ~412 B, ~20:1).
        // Writing the identical array uncompressed cancels that shared
        // overhead, isolating the deflate saving on the data chunk.
        let raw_path = temp_path("hdf5_deflate_raw");
        {
            let mut raw_writer = Hdf5Writer::new();
            raw_writer.set_compression_type(COMPRESS_NONE);
            raw_writer
                .open_file(&raw_path, NDFileOpenMode::WRITE, &arr)
                .unwrap();
            raw_writer.write_file(&Arc::new(arr)).unwrap();
            raw_writer.close_file().unwrap();
        }
        let compressed_size = std::fs::metadata(&path).unwrap().len();
        let raw_size = std::fs::metadata(&raw_path).unwrap().len();
        assert!(
            compressed_size + 4096 < raw_size,
            "deflate must shrink the data chunk: compressed={compressed_size} raw={raw_size}"
        );

        let h5file = H5File::open(&path).unwrap();
        let ds = h5file.dataset("entry/instrument/detector/data").unwrap();
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
        if let NDDataViewMut::U8(v) = arr.data_mut().view_mut() {
            for i in 0..v.len() {
                v[i] = (i % 4) as u8;
            }
        }

        writer
            .open_file(&path, NDFileOpenMode::WRITE, &arr)
            .unwrap();
        writer.write_file(&Arc::new(arr)).unwrap();
        writer.close_file().unwrap();

        let h5file = H5File::open(&path).unwrap();
        let ds = h5file.dataset("entry/instrument/detector/data").unwrap();
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
        if let NDDataViewMut::U16(v) = arr.data_mut().view_mut() {
            for i in 0..v.len() {
                v[i] = (i % 8) as u16;
            }
        }

        writer
            .open_file(&path, NDFileOpenMode::WRITE, &arr)
            .unwrap();
        writer.write_file(&Arc::new(arr)).unwrap();
        writer.close_file().unwrap();

        let h5file = H5File::open(&path).unwrap();
        let ds = h5file.dataset("entry/instrument/detector/data").unwrap();
        let data: Vec<u16> = ds.read_raw().unwrap();
        assert_eq!(data.len(), 64 * 64);
        assert_eq!(data[0], 0);
        assert_eq!(data[9], 1);

        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn test_szip_uses_nearest_neighbor_mask() {
        // C uses H5_SZIP_NN_OPTION_MASK (32); the SZIP filter's cd_values[0]
        // must declare NN coding, and the data must round-trip.
        let mut writer = Hdf5Writer::new();
        writer.set_compression_type(COMPRESS_SZIP);
        writer.read_settings(); // as open_file does
        let pipeline = writer
            .cfg
            .build_pipeline(1, &mono_frame())
            .expect("szip pipeline");
        assert_eq!(pipeline.filters[0].cd_values[0], SZIP_NN_OPTION_MASK);

        let path = temp_path("hdf5_szip_nn");
        let mut arr = NDArray::new(
            vec![NDDimension::new(64), NDDimension::new(64)],
            NDDataType::UInt8,
        );
        if let NDDataViewMut::U8(v) = arr.data_mut().view_mut() {
            for (i, e) in v.iter_mut().enumerate() {
                *e = (i % 13) as u8;
            }
        }
        writer
            .open_file(&path, NDFileOpenMode::WRITE, &arr)
            .unwrap();
        writer.write_file(&Arc::new(arr)).unwrap();
        writer.close_file().unwrap();

        let h5file = H5File::open(&path).unwrap();
        let ds = h5file.dataset("entry/instrument/detector/data").unwrap();
        let data: Vec<u8> = ds.read_raw().unwrap();
        assert_eq!(data.len(), 64 * 64);
        assert_eq!(data[20], (20 % 13) as u8);
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn test_blosc_default_shuffle_is_byte_shuffle() {
        // C default bloscShuffleType=1 (byte shuffle), NDFileHDF5.cpp:2346;
        // BLOSC cd_values[5] carries the shuffle type.
        let mut writer = Hdf5Writer::new();
        writer.set_compression_type(COMPRESS_BLOSC);
        writer.read_settings(); // as open_file does
        let pipeline = writer
            .cfg
            .build_pipeline(2, &mono_frame())
            .expect("blosc pipeline");
        assert_eq!(pipeline.filters[0].cd_values[5], 1);
    }

    #[test]
    fn test_nbit_packs_to_reduced_precision_datatype() {
        // C's N-bit codec (NDFileHDF5.cpp:3355-3357) narrows the dataset
        // datatype (H5Tset_precision/H5Tset_offset) and registers a
        // parameterless H5Pset_nbit filter. The standard write path reproduces
        // that with rust-hdf5 0.2.22: `DatasetBuilder::datatype` stores a
        // reduced-precision `FixedPoint` and `FilterPipeline::nbit` packs to it,
        // so the file is byte-readable by h5py/libhdf5. `build_pipeline` still
        // returns None for N-bit because the SWMR streaming builder cannot
        // override the datatype — packing is applied out of band by the standard
        // path only.
        let mut writer = Hdf5Writer::new();
        writer.set_compression_type(COMPRESS_NBIT);
        writer.set_nbit_precision(10);
        writer.set_nbit_offset(0);
        writer.read_settings(); // as open_file does
        assert!(
            writer.cfg.build_pipeline(2, &mono_frame()).is_none(),
            "N-bit packing is applied via a datatype override, not build_pipeline"
        );

        let path = temp_path("hdf5_nbit_packed");
        let mut arr = NDArray::new(
            vec![NDDimension::new(8), NDDimension::new(8)],
            NDDataType::UInt16,
        );
        if let NDDataViewMut::U16(v) = arr.data_mut().view_mut() {
            for (i, x) in v.iter_mut().enumerate() {
                *x = (i as u16 * 7) & 0x3FF; // within the 10-bit range
            }
            v[0] = 0xFFFF; // above 10 bits: must pack to the low 10 bits (0x3FF)
        }
        writer
            .open_file(&path, NDFileOpenMode::WRITE, &arr)
            .unwrap();
        writer.write_file(&Arc::new(arr)).unwrap();
        writer.close_file().unwrap();

        let h5file = H5File::open(&path).unwrap();
        let ds = h5file.dataset("entry/instrument/detector/data").unwrap();

        // The on-disk datatype carries the reduced precision (C-observable
        // narrower datatype) within the unchanged 2-byte footprint.
        match ds.datatype().unwrap() {
            DatatypeMessage::FixedPoint {
                size,
                bit_precision,
                bit_offset,
                signed,
                ..
            } => {
                assert_eq!(size, 2, "byte footprint unchanged");
                assert_eq!(bit_precision, 10, "precision narrowed to 10 bits");
                assert_eq!(bit_offset, 0);
                assert!(!signed, "u16 is unsigned");
            }
            other => panic!("expected reduced-precision FixedPoint, got {other:?}"),
        }

        let data: Vec<u16> = ds.read_raw().unwrap();
        assert_eq!(data.len(), 8 * 8);
        // Real bit-packing: the over-range first element truncates to 10 bits.
        assert_eq!(data[0], 0x3FF, "0xFFFF must pack to the low 10 bits");
        for i in 1..data.len() {
            assert_eq!(
                data[i],
                (i as u16 * 7) & 0x3FF,
                "in-range value {i} round-trips"
            );
        }
        std::fs::remove_file(&path).ok();
    }

    /// A precision or offset past the element widens the nbit datatype
    /// (H5Tprecis.c, H5Toffset.c); the frame is stored in that width, each
    /// value extended, and the fill with it (see `widen_le`).
    #[test]
    fn a_widened_nbit_datatype_stores_the_extended_values() {
        use rust_hdf5::format::messages::filter::FILTER_NBIT;
        let cases = [
            (
                NDDataType::UInt16,
                20,
                0,
                NDDataBuffer::from_vec::<u16>(vec![0xFFFF, 1, 0x8000, 0]),
                513.0,
                [1, 2, 0],
            ),
            (
                NDDataType::Int16,
                16,
                4,
                NDDataBuffer::from_vec::<i16>(vec![-3, 1, i16::MIN, 0]),
                -2.0,
                [0xFE, 0xFF, 0xFF],
            ),
        ];
        for (data_type, precision, offset, data, fill, fill_bytes) in cases {
            let what = format!("{data_type:?} precision {precision} offset {offset}");
            let path = temp_path("hdf5_nbit_widened");
            let mut writer = Hdf5Writer::new();
            writer.set_compression_type(COMPRESS_NBIT);
            writer.set_nbit_precision(precision);
            writer.set_nbit_offset(offset);
            writer.set_fill_value(fill);
            let arr =
                NDArray::with_data(vec![NDDimension::new(2), NDDimension::new(2)], data.clone());
            writer
                .open_file(&path, NDFileOpenMode::WRITE, &arr)
                .unwrap();
            writer.write_file(&Arc::new(arr)).unwrap();
            writer.close_file().unwrap();

            let h5file = H5File::open(&path).unwrap();
            let ds = h5file.dataset("entry/instrument/detector/data").unwrap();
            assert_eq!(ds.datatype().unwrap().element_size(), 3, "{what}");
            assert_eq!(
                ds.filters()
                    .unwrap()
                    .iter()
                    .map(|f| f.id)
                    .collect::<Vec<_>>(),
                [FILTER_NBIT],
                "{what}"
            );
            // The filter stores bits [offset, offset+precision) of each
            // widened element; the chunk is decoded here because a read
            // converts the field to its value.
            let keep = ((1u32 << precision) - 1) << offset;
            let (chunk, _) = ds.read_chunk_raw_at(&[0, 0]).unwrap();
            let cd_values = &ds.filters().unwrap()[0].cd_values;
            let decoded =
                rust_hdf5::format::nbit_scaleoffset::apply_nbit(&chunk, cd_values, false).unwrap();
            let widened = widen_le(nd_buffer_to_le_bytes(&data), data_type, 3);
            let want: Vec<u8> = widened
                .as_chunks::<3>()
                .0
                .iter()
                .flat_map(|&[a, b, c]| {
                    let v = u32::from_le_bytes([a, b, c, 0]) & keep;
                    v.to_le_bytes()[..3].to_vec()
                })
                .collect();
            assert_eq!(decoded, want, "{what}");
            assert_eq!(
                ds.fill_value().unwrap(),
                rust_hdf5::FillValue::UserDefined(fill_bytes.to_vec()),
                "{what}"
            );
            std::fs::remove_file(&path).ok();
        }
    }

    /// C casts HDF5_fillValue to the element type, `(epicsUInt16)-1.0` is
    /// 0xFFFF, where Rust's `as` saturates to 0 (NDFileHDF5.cpp:3500-3537).
    #[test]
    fn the_fill_value_takes_the_c_cast() {
        for swmr in [false, true] {
            for (data_type, fill, want) in [
                (NDDataType::UInt16, -1.0, vec![0xFF, 0xFF]),
                (NDDataType::UInt8, 300.0, vec![44]),
                (NDDataType::Int8, 200.0, vec![0xC8]),
                (NDDataType::UInt16, 70000.5, vec![0x70, 0x11]),
            ] {
                let what = format!("swmr {swmr} {data_type:?} fill {fill}");
                let path = temp_path("hdf5_fill_cast");
                let mut writer = Hdf5Writer::new();
                writer.set_swmr_mode(swmr);
                writer.set_fill_value(fill);
                let arr = NDArray::new(vec![NDDimension::new(2), NDDimension::new(2)], data_type);
                let mode = if swmr {
                    NDFileOpenMode::WRITE | NDFileOpenMode::MULTIPLE
                } else {
                    NDFileOpenMode::WRITE
                };
                writer.open_file(&path, mode, &arr).unwrap();
                writer.write_file(&Arc::new(arr)).unwrap();
                writer.close_file().unwrap();
                let h5file = H5File::open(&path).unwrap();
                let ds = h5file.dataset("entry/instrument/detector/data").unwrap();
                assert_eq!(
                    ds.fill_value().unwrap(),
                    rust_hdf5::FillValue::UserDefined(want),
                    "{what}"
                );
                std::fs::remove_file(&path).ok();
            }
        }
    }

    /// C's SWMR dataset takes the fill value from the dcpl every dataset
    /// shares (NDFileHDF5.cpp:3895), in the stored width.
    #[test]
    fn a_swmr_dataset_carries_the_fill_value() {
        for (compression, precision, fill) in [
            (COMPRESS_NONE, 0, vec![7, 0]),
            (COMPRESS_NBIT, 20, vec![7, 0, 0]),
        ] {
            let path = temp_path("hdf5_swmr_fill");
            let mut writer = Hdf5Writer::new();
            writer.set_swmr_mode(true);
            writer.set_compression_type(compression);
            writer.set_nbit_precision(precision);
            writer.set_fill_value(7.0);
            let arr = NDArray::new(
                vec![NDDimension::new(2), NDDimension::new(2)],
                NDDataType::UInt16,
            );
            writer
                .open_file(
                    &path,
                    NDFileOpenMode::WRITE | NDFileOpenMode::MULTIPLE,
                    &arr,
                )
                .unwrap();
            writer.write_file(&Arc::new(arr)).unwrap();
            writer.close_file().unwrap();
            let h5file = H5File::open(&path).unwrap();
            let ds = h5file.dataset("entry/instrument/detector/data").unwrap();
            assert_eq!(
                ds.fill_value().unwrap(),
                rust_hdf5::FillValue::UserDefined(fill),
                "compression {compression}"
            );
            std::fs::remove_file(&path).ok();
        }
    }

    /// A SWMR file stores the nbit datatype under its filter as a standard
    /// one does (NDFileHDF5.cpp:3362-3364), narrowed or widened.
    #[test]
    fn a_swmr_file_stores_the_nbit_datatype_under_its_filter() {
        use rust_hdf5::format::messages::filter::FILTER_NBIT;
        let data = NDDataBuffer::from_vec::<u16>(vec![0xFFFF, 1, 0x8008, 0x3FF]);
        for (precision, offset, size) in [(10, 3, 2), (20, 0, 3)] {
            let what = format!("precision {precision} offset {offset}");
            let path = temp_path("hdf5_swmr_nbit");
            let mut writer = Hdf5Writer::new();
            writer.set_swmr_mode(true);
            writer.set_compression_type(COMPRESS_NBIT);
            writer.set_nbit_precision(precision);
            writer.set_nbit_offset(offset);
            let arr =
                NDArray::with_data(vec![NDDimension::new(2), NDDimension::new(2)], data.clone());
            writer
                .open_file(
                    &path,
                    NDFileOpenMode::WRITE | NDFileOpenMode::MULTIPLE,
                    &arr,
                )
                .unwrap();
            writer.write_file(&Arc::new(arr)).unwrap();
            writer.close_file().unwrap();

            let h5file = H5File::open(&path).unwrap();
            let ds = h5file.dataset("entry/instrument/detector/data").unwrap();
            assert_eq!(ds.datatype().unwrap().element_size(), size, "{what}");
            let filters = ds.filters().unwrap();
            assert_eq!(
                filters.iter().map(|f| f.id).collect::<Vec<_>>(),
                [FILTER_NBIT],
                "{what}"
            );
            let (chunk, _) = ds.read_chunk_raw_at(&[0, 0, 0]).unwrap();
            let decoded = rust_hdf5::format::nbit_scaleoffset::apply_nbit(
                &chunk,
                &filters[0].cd_values,
                false,
            )
            .unwrap();
            let keep = ((1u64 << precision) - 1) << offset;
            let want: Vec<u8> = widen_le(
                nd_buffer_to_le_bytes(&data),
                NDDataType::UInt16,
                size as usize,
            )
            .chunks(size as usize)
            .flat_map(|e| {
                let mut b = [0u8; 8];
                b[..e.len()].copy_from_slice(e);
                (u64::from_le_bytes(b) & keep).to_le_bytes()[..e.len()].to_vec()
            })
            .collect();
            assert_eq!(decoded, want, "{what}");
            std::fs::remove_file(&path).ok();
        }
    }

    /// C registers `H5Pset_nbit` whatever `H5Tset_precision` refused
    /// (NDFileHDF5.cpp:3362-3364): precision 0, or a float narrower than its
    /// type, leaves the native datatype under a full-width nbit filter.
    #[test]
    fn a_refused_nbit_precision_keeps_the_filter_at_full_width() {
        use rust_hdf5::format::messages::filter::FILTER_NBIT;
        let cases = [
            (NDDataType::UInt16, 0),
            (NDDataType::Float32, 0),
            (NDDataType::Float32, 16),
            (NDDataType::Float64, 40),
        ];
        for (data_type, precision) in cases {
            let path = temp_path("hdf5_nbit_full_width");
            let mut writer = Hdf5Writer::new();
            writer.set_compression_type(COMPRESS_NBIT);
            writer.set_nbit_precision(precision);
            writer.set_nbit_offset(0);
            let mut arr = NDArray::new(vec![NDDimension::new(4), NDDimension::new(4)], data_type);
            let want: Vec<f64> = (0..16).map(|i| -1.5e3 + i as f64 * 777.25).collect();
            match arr.data_mut().view_mut() {
                NDDataViewMut::U16(v) => {
                    v.iter_mut().zip(0u16..).for_each(|(x, i)| *x = 0xFFFF - i)
                }
                NDDataViewMut::F32(v) => v.iter_mut().zip(&want).for_each(|(x, w)| *x = *w as f32),
                NDDataViewMut::F64(v) => v.copy_from_slice(&want),
                _ => unreachable!(),
            }
            let written = arr.data().clone();
            writer
                .open_file(&path, NDFileOpenMode::WRITE, &arr)
                .unwrap();
            writer.write_file(&Arc::new(arr)).unwrap();
            writer.close_file().unwrap();

            let what = format!("{data_type:?} precision {precision}");
            let h5file = H5File::open(&path).unwrap();
            let ds = h5file.dataset("entry/instrument/detector/data").unwrap();
            let native = match data_type {
                NDDataType::UInt16 => <u16 as rust_hdf5::types::H5Type>::hdf5_type(),
                NDDataType::Float32 => <f32 as rust_hdf5::types::H5Type>::hdf5_type(),
                _ => <f64 as rust_hdf5::types::H5Type>::hdf5_type(),
            };
            assert_eq!(ds.datatype().unwrap(), native, "{what}");
            let filters = ds.filters().unwrap();
            assert_eq!(
                filters.iter().map(|f| f.id).collect::<Vec<_>>(),
                [FILTER_NBIT],
                "{what}"
            );
            assert_eq!(filters[0].cd_values[1], 1, "{what}: need_not_compress");
            let back = match written.view() {
                NDDataView::U16(v) => ds.read_raw::<u16>().unwrap() == v,
                NDDataView::F32(v) => ds.read_raw::<f32>().unwrap() == v,
                NDDataView::F64(v) => ds.read_raw::<f64>().unwrap() == v,
                _ => unreachable!(),
            };
            assert!(back, "{what}: values round-trip");
            std::fs::remove_file(&path).ok();
        }
    }

    #[test]
    fn test_chunk_geometry_recorded() {
        // The requested frame/row/col chunk geometry is the on-disk chunk.
        let path = temp_path("hdf5_chunkgeom");
        let mut writer = Hdf5Writer::new();
        writer.set_num_capture(0);
        writer.set_chunk_size_auto(false);
        writer.set_n_row_chunks(4);
        writer.set_n_col_chunks(2);
        writer.set_n_frames_chunks(3);

        let mut arr = NDArray::new(
            vec![NDDimension::new(8), NDDimension::new(8)],
            NDDataType::UInt16,
        );
        if let NDDataViewMut::U16(v) = arr.data_mut().view_mut() {
            for i in 0..v.len() {
                v[i] = i as u16;
            }
        }

        writer
            .open_file(
                &path,
                NDFileOpenMode::WRITE | NDFileOpenMode::MULTIPLE,
                &arr,
            )
            .unwrap();
        writer.write_file(&Arc::new(arr.clone())).unwrap();
        writer.write_file(&Arc::new(arr)).unwrap();
        writer.close_file().unwrap();

        let h5 = H5File::open(&path).unwrap();
        let ds = h5.dataset("entry/instrument/detector/data").unwrap();
        assert_eq!(ds.shape(), vec![2, 8, 8]);
        // Data still round-trips correctly through the per-frame chunks.
        let data: Vec<u16> = ds.read_raw().unwrap();
        assert_eq!(data.len(), 2 * 64);
        for i in 0..64usize {
            assert_eq!(data[i], i as u16, "frame0 element {}", i);
            assert_eq!(data[64 + i], i as u16, "frame1 element {}", i);
        }
        assert_eq!(ds.chunk_dims(), Some(vec![3, 4, 2]));

        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn test_extra_dimensions_layout() {
        // HDF5_nExtraDims=2 builds 2+1 fixed leading axes. With the param sizes
        // extraDimSizeN(=eds[0])=2, extraDimSizeX(=eds[1])=3, extraDimSizeY(=
        // eds[2])=4 the dataspace is rank-5 `{Y, X, N, frameY, frameX}` =
        // [4,3,2,4,4] — C `NDFileHDF5::configureDims` order
        // (NDFileHDF5.cpp:3121-3230; docs/ADCore/NDFileHDF5.rst:379-380). The
        // frame data is row-major identical to the collapsed [24,4,4] form
        // (the innermost leading axis "N" varies fastest), so frame `f` lands
        // at flat leading index `f`.
        let path = temp_path("hdf5_extradims");
        let mut writer = Hdf5Writer::new();
        writer.set_n_extra_dims(2);
        writer.set_extra_dim_size(0, 2); // N: frames per point
        writer.set_extra_dim_size(1, 3); // X
        writer.set_extra_dim_size(2, 4); // Y
        writer.set_extra_dim_name(0, "n");
        writer.set_extra_dim_name(1, "scanX");
        writer.set_extra_dim_name(2, "scanY");

        let mut arr = NDArray::new(
            vec![NDDimension::new(4), NDDimension::new(4)],
            NDDataType::UInt16,
        );
        for f in 0..24u16 {
            if let NDDataViewMut::U16(v) = arr.data_mut().view_mut() {
                for x in v.iter_mut() {
                    *x = f;
                }
            }
            if f == 0 {
                writer
                    .open_file(
                        &path,
                        NDFileOpenMode::WRITE | NDFileOpenMode::MULTIPLE,
                        &arr,
                    )
                    .unwrap();
            }
            writer.write_file(&Arc::new(arr.clone())).unwrap();
        }
        writer.close_file().unwrap();

        let h5 = H5File::open(&path).unwrap();
        let ds = h5.dataset("entry/instrument/detector/data").unwrap();
        // Rank-5 multi-extra-dimension dataspace: [Y, X, N, frameY, frameX].
        assert_eq!(ds.shape(), vec![4, 3, 2, 4, 4]);
        let data: Vec<u16> = ds.read_raw().unwrap();
        assert_eq!(data.len(), 24 * 16);
        for f in 0..24usize {
            for i in 0..16usize {
                assert_eq!(data[f * 16 + i], f as u16, "frame {} elem {}", f, i);
            }
        }
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn test_extra_dimensions_one_virtual() {
        // HDF5_nExtraDims=1 builds 1+1 fixed leading axes:
        // {X, N, frameY, frameX}. With eds[0]=N=2, eds[1]=X=3 the dataspace is
        // rank-4 [3,2,Y,X] (NDFileHDF5.rst:377-378). 6 frames, value==frame.
        let path = temp_path("hdf5_extradims_1");
        let mut writer = Hdf5Writer::new();
        writer.set_n_extra_dims(1);
        writer.set_extra_dim_size(0, 2); // N: frames per point
        writer.set_extra_dim_size(1, 3); // X

        let mut arr = NDArray::new(
            vec![NDDimension::new(4), NDDimension::new(4)],
            NDDataType::UInt16,
        );
        for f in 0..6u16 {
            if let NDDataViewMut::U16(v) = arr.data_mut().view_mut() {
                for x in v.iter_mut() {
                    *x = f;
                }
            }
            if f == 0 {
                writer
                    .open_file(
                        &path,
                        NDFileOpenMode::WRITE | NDFileOpenMode::MULTIPLE,
                        &arr,
                    )
                    .unwrap();
            }
            writer.write_file(&Arc::new(arr.clone())).unwrap();
        }
        writer.close_file().unwrap();

        let h5 = H5File::open(&path).unwrap();
        let ds = h5.dataset("entry/instrument/detector/data").unwrap();
        assert_eq!(ds.shape(), vec![3, 2, 4, 4]);
        let data: Vec<u16> = ds.read_raw().unwrap();
        assert_eq!(data.len(), 6 * 16);
        for f in 0..6usize {
            for i in 0..16usize {
                assert_eq!(data[f * 16 + i], f as u16, "frame {} elem {}", f, i);
            }
        }

        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn test_swmr_extra_dimensions_grid_layout() {
        // SWMR mirror of `test_extra_dimensions_layout`: HDF5_nExtraDims=2 with
        // eds[0]=2,eds[1]=3,eds[2]=4 must build the same rank-5 fixed grid
        // [Y,X,N,frameY,frameX] = [4,3,2,4,4] as the standard path (C
        // configureDims), not the old collapsed single leading axis. Each frame
        // is placed at its odometer chunk position via write_chunk_at; frame `f`
        // lands at flat leading index `f` (innermost "N" varies fastest).
        let path = temp_path("hdf5_swmr_extradims");
        let mut writer = Hdf5Writer::new();
        writer.set_swmr_mode(true);
        writer.set_n_extra_dims(2);
        writer.set_extra_dim_size(0, 2); // N: frames per point
        writer.set_extra_dim_size(1, 3); // X
        writer.set_extra_dim_size(2, 4); // Y

        let mut arr = NDArray::new(
            vec![NDDimension::new(4), NDDimension::new(4)],
            NDDataType::UInt16,
        );
        for f in 0..24u16 {
            if let NDDataViewMut::U16(v) = arr.data_mut().view_mut() {
                for x in v.iter_mut() {
                    *x = f;
                }
            }
            if f == 0 {
                writer
                    .open_file(
                        &path,
                        NDFileOpenMode::WRITE | NDFileOpenMode::MULTIPLE,
                        &arr,
                    )
                    .unwrap();
            }
            writer.write_file(&Arc::new(arr.clone())).unwrap();
        }
        writer.close_file().unwrap();

        let mut reader = rust_hdf5::swmr::SwmrFileReader::open(&path).unwrap();
        assert_eq!(
            reader
                .dataset_shape("entry/instrument/detector/data")
                .unwrap(),
            vec![4, 3, 2, 4, 4]
        );
        let data: Vec<u16> = reader
            .read_dataset("entry/instrument/detector/data")
            .unwrap();
        assert_eq!(data.len(), 24 * 16);
        for f in 0..24usize {
            for i in 0..16usize {
                assert_eq!(data[f * 16 + i], f as u16, "frame {} elem {}", f, i);
            }
        }
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn test_swmr_extra_dimensions_grid_subtiled() {
        // SWMR grid with frame sub-tiling: HDF5_nExtraDims=1 (rank-4 [3,2,4,4])
        // and HDF5_nRowChunks=2 splits each 4-row frame into two row tiles, so
        // the grid write path emits two write_chunk_at tiles per frame. The
        // reassembled pixels must still be row-major-correct. 6 frames fill the
        // 3x2 grid exactly; value==frame.
        let path = temp_path("hdf5_swmr_extradims_tiled");
        let mut writer = Hdf5Writer::new();
        writer.set_swmr_mode(true);
        writer.set_n_extra_dims(1);
        writer.set_extra_dim_size(0, 2); // N
        writer.set_extra_dim_size(1, 3); // X
        writer.set_n_row_chunks(2); // split the 4-row frame into 2 tiles

        let mut arr = NDArray::new(
            vec![NDDimension::new(4), NDDimension::new(4)],
            NDDataType::UInt16,
        );
        for f in 0..6u16 {
            if let NDDataViewMut::U16(v) = arr.data_mut().view_mut() {
                // Distinct per-element values prove tile reassembly, not just a
                // constant: value = frame*100 + (row*4 + col).
                for (i, x) in v.iter_mut().enumerate() {
                    *x = f * 100 + i as u16;
                }
            }
            if f == 0 {
                writer
                    .open_file(
                        &path,
                        NDFileOpenMode::WRITE | NDFileOpenMode::MULTIPLE,
                        &arr,
                    )
                    .unwrap();
            }
            writer.write_file(&Arc::new(arr.clone())).unwrap();
        }
        writer.close_file().unwrap();

        let mut reader = rust_hdf5::swmr::SwmrFileReader::open(&path).unwrap();
        assert_eq!(
            reader
                .dataset_shape("entry/instrument/detector/data")
                .unwrap(),
            vec![3, 2, 4, 4]
        );
        let data: Vec<u16> = reader
            .read_dataset("entry/instrument/detector/data")
            .unwrap();
        assert_eq!(data.len(), 6 * 16);
        for f in 0..6usize {
            for i in 0..16usize {
                assert_eq!(
                    data[f * 16 + i],
                    f as u16 * 100 + i as u16,
                    "frame {} elem {}",
                    f,
                    i
                );
            }
        }
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn test_swmr_extra_dimensions_grid_partial_fill() {
        // SWMR grid written with fewer frames than the grid capacity: the
        // extent grows as C's `dims_` does, to [2,2,4,4] (C oracle), and the
        // frames sit at their odometer positions.
        let path = temp_path("hdf5_swmr_extradims_partial");
        let mut writer = Hdf5Writer::new();
        writer.set_swmr_mode(true);
        writer.set_n_extra_dims(1);
        writer.set_extra_dim_size(0, 2); // N
        writer.set_extra_dim_size(1, 3); // X

        let mut arr = NDArray::new(
            vec![NDDimension::new(4), NDDimension::new(4)],
            NDDataType::UInt16,
        );
        for f in 0..4u16 {
            if let NDDataViewMut::U16(v) = arr.data_mut().view_mut() {
                for x in v.iter_mut() {
                    *x = f + 1; // non-zero so fill (0) is distinguishable
                }
            }
            if f == 0 {
                writer
                    .open_file(
                        &path,
                        NDFileOpenMode::WRITE | NDFileOpenMode::MULTIPLE,
                        &arr,
                    )
                    .unwrap();
            }
            writer.write_file(&Arc::new(arr.clone())).unwrap();
        }
        writer.close_file().unwrap();

        let mut reader = rust_hdf5::swmr::SwmrFileReader::open(&path).unwrap();
        assert_eq!(
            reader
                .dataset_shape("entry/instrument/detector/data")
                .unwrap(),
            vec![2, 2, 4, 4]
        );
        let data: Vec<u16> = reader
            .read_dataset("entry/instrument/detector/data")
            .unwrap();
        assert_eq!(data.len(), 4 * 16);
        for f in 0..4usize {
            for i in 0..16usize {
                assert_eq!(data[f * 16 + i], f as u16 + 1, "frame {} elem {}", f, i);
            }
        }
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn test_swmr_streaming() {
        let path = temp_path("hdf5_swmr");
        let mut writer = Hdf5Writer::new();
        writer.set_swmr_mode(true);
        writer.set_num_capture(0);
        writer.set_flush_nth_frame(2);

        let arr = NDArray::new(
            vec![NDDimension::new(8), NDDimension::new(8)],
            NDDataType::Float32,
        );

        writer
            .open_file(
                &path,
                NDFileOpenMode::WRITE | NDFileOpenMode::MULTIPLE,
                &arr,
            )
            .unwrap();
        writer.write_file(&Arc::new(arr.clone())).unwrap();
        writer.write_file(&Arc::new(arr.clone())).unwrap(); // should trigger flush
        writer.write_file(&Arc::new(arr)).unwrap();
        writer.close_file().unwrap();

        assert_eq!(writer.frame_count(), 3);

        // Read back via SwmrFileReader
        let mut reader = rust_hdf5::swmr::SwmrFileReader::open(&path).unwrap();
        let shape = reader
            .dataset_shape("entry/instrument/detector/data")
            .unwrap();
        assert_eq!(shape[0], 3); // 3 frames
        assert_eq!(shape[1], 8);
        assert_eq!(shape[2], 8);

        let data: Vec<f32> = reader
            .read_dataset("entry/instrument/detector/data")
            .unwrap();
        assert_eq!(data.len(), 3 * 8 * 8);

        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn test_swmr_compression_is_applied() {
        // rust-hdf5 0.2.15 exposes a filtered SWMR dataset constructor, so
        // SWMR + compression produces a genuinely compressed file — the
        // compression is NOT dropped, and the data round-trips.
        let path = temp_path("hdf5_swmr_comp");
        let mut writer = Hdf5Writer::new();
        writer.set_swmr_mode(true);
        writer.set_num_capture(0);
        writer.set_compression_type(COMPRESS_ZLIB);

        let arr = NDArray::new(
            vec![NDDimension::new(8), NDDimension::new(8)],
            NDDataType::UInt16,
        );
        writer
            .open_file(
                &path,
                NDFileOpenMode::WRITE | NDFileOpenMode::MULTIPLE,
                &arr,
            )
            .unwrap();
        assert!(
            !writer.swmr_compression_dropped(),
            "SWMR+ZLIB must apply compression, not drop it"
        );
        writer.write_file(&Arc::new(arr.clone())).unwrap();
        writer.write_file(&Arc::new(arr)).unwrap();
        writer.close_file().unwrap();

        // The compressed SWMR dataset round-trips.
        let mut reader = rust_hdf5::swmr::SwmrFileReader::open(&path).unwrap();
        let shape = reader
            .dataset_shape("entry/instrument/detector/data")
            .unwrap();
        assert_eq!(shape, vec![2, 8, 8]);
        let data: Vec<u16> = reader
            .read_dataset("entry/instrument/detector/data")
            .unwrap();
        assert_eq!(data.len(), 2 * 8 * 8);

        std::fs::remove_file(&path).ok();
    }

    /// C's dataspace does not depend on compression, so a compressed SWMR
    /// file with extra dims is the same fixed grid, filtered.
    #[test]
    fn a_compressed_swmr_file_keeps_the_extra_dimension_grid() {
        let path = temp_path("hdf5_swmr_comp_grid");
        let mut writer = Hdf5Writer::new();
        writer.set_swmr_mode(true);
        writer.set_compression_type(COMPRESS_ZLIB);
        writer.set_n_extra_dims(1);
        writer.set_extra_dim_size(0, 2);
        writer.set_extra_dim_size(1, 3);
        let mut arr = NDArray::new(
            vec![NDDimension::new(4), NDDimension::new(4)],
            NDDataType::UInt16,
        );
        for f in 0..6u16 {
            if let NDDataViewMut::U16(v) = arr.data_mut().view_mut() {
                v.fill(f);
            }
            if f == 0 {
                writer
                    .open_file(
                        &path,
                        NDFileOpenMode::WRITE | NDFileOpenMode::MULTIPLE,
                        &arr,
                    )
                    .unwrap();
            }
            writer.write_file(&Arc::new(arr.clone())).unwrap();
        }
        writer.close_file().unwrap();

        let h5file = H5File::open(&path).unwrap();
        let ds = h5file.dataset("entry/instrument/detector/data").unwrap();
        assert_eq!(ds.shape(), vec![3, 2, 4, 4]);
        assert_eq!(
            ds.filters()
                .unwrap()
                .iter()
                .map(|f| f.id)
                .collect::<Vec<_>>(),
            [rust_hdf5::format::messages::filter::FILTER_DEFLATE]
        );
        let data = ds.read_raw::<u16>().unwrap();
        for f in 0..6usize {
            assert!(
                data[f * 16..][..16].iter().all(|&v| v == f as u16),
                "frame {f}"
            );
        }
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn test_layout_xml_param() {
        // C createFileLayout (NDFileHDF5.cpp:3898-3939) loads at open: a
        // good file, inline XML and an empty spec load; a bad or missing
        // file fails the open.
        let mut writer = Hdf5Writer::new();
        let dir = std::env::temp_dir();
        let good = dir.join(format!("adcore_layout_good_{}.xml", std::process::id()));
        std::fs::write(
            &good,
            r#"<hdf5_layout><group name="entry"><dataset name="data" source="detector" det_default="true"/></group></hdf5_layout>"#,
        )
        .unwrap();
        writer.set_layout_filename(good.to_str().unwrap());
        writer.load_layout().unwrap();
        assert_eq!(writer.layout.as_ref().unwrap().groups[0].name, "entry");

        writer.set_layout_filename("");
        writer.load_layout().unwrap();
        assert!(writer.layout.is_some(), "the default layout");

        let bad = dir.join(format!("adcore_layout_bad_{}.xml", std::process::id()));
        std::fs::write(&bad, r#"<not_a_layout/>"#).unwrap();
        writer.set_layout_filename(bad.to_str().unwrap());
        assert!(writer.load_layout().is_err());
        assert!(writer.layout.is_none());

        writer.set_layout_filename(dir.join("adcore_layout_missing.xml").to_str().unwrap());
        assert!(writer.load_layout().is_err());

        std::fs::remove_file(&good).ok();
        std::fs::remove_file(&bad).ok();
    }

    #[test]
    fn test_layout_xml_places_dataset_in_nested_tree() {
        // A valid layout XML must place the image dataset at the layout's
        // det_default path (C ADCore /entry/instrument/detector/data),
        // NDAttributes under the ndattr_default group, and the performance
        // dataset under the group holding the `timestamp` dataset — NOT flat
        // at the file root.
        let dir = std::env::temp_dir();
        let layout = dir.join(format!("adcore_layout_nested_{}.xml", std::process::id()));
        std::fs::write(
            &layout,
            r#"<hdf5_layout>
              <group name="entry">
                <group name="instrument">
                  <group name="detector">
                    <dataset name="data" source="detector" det_default="true">
                      <attribute name="signal" source="constant" value="1" type="int"/>
                    </dataset>
                  </group>
                  <group name="NDAttributes" ndattr_default="true"/>
                  <group name="performance">
                    <dataset name="timestamp"/>
                  </group>
                </group>
              </group>
            </hdf5_layout>"#,
        )
        .unwrap();

        let path = temp_path("hdf5_layout_nested");
        let mut writer = Hdf5Writer::new();
        writer.set_num_capture(0);
        writer.set_store_performance(true);
        writer.set_layout_filename(layout.to_str().unwrap());

        let mk = |fill: f64| {
            let mut arr = NDArray::new(
                vec![NDDimension::new(4), NDDimension::new(4)],
                NDDataType::UInt16,
            );
            arr.attributes.add(NDAttribute::new_static(
                "exposure",
                "",
                NDAttrSource::Driver,
                NDAttrValue::Float64(fill),
            ));
            arr
        };

        let a0 = mk(0.5);
        writer
            .open_file(&path, NDFileOpenMode::WRITE | NDFileOpenMode::MULTIPLE, &a0)
            .unwrap();
        writer.write_file(&Arc::new(a0)).unwrap();
        writer.write_file(&Arc::new(mk(0.75))).unwrap();
        writer.close_file().unwrap();

        let h5 = H5File::open(&path).unwrap();
        let names = h5.dataset_names();
        // Image dataset at the nested layout path, NOT flat `data`.
        assert!(
            names.contains(&"entry/instrument/detector/data".to_string()),
            "image dataset must be at the nested layout path; got {:?}",
            names
        );
        assert!(
            !names.contains(&"data".to_string()),
            "must not also write a flat-root `data` dataset"
        );
        let img = h5.dataset("entry/instrument/detector/data").unwrap();
        assert_eq!(img.shape(), vec![2, 4, 4]);
        // Layout constant attribute materialised.
        assert_eq!(
            img.attr("signal").unwrap().read_numeric::<i32>().unwrap(),
            1
        );
        // NDAttribute dataset under the ndattr_default group.
        assert!(
            names.contains(&"entry/instrument/NDAttributes/exposure".to_string()),
            "NDAttribute dataset must be under the layout ndattr group; got {:?}",
            names
        );
        // Performance dataset under the layout's performance group.
        assert!(
            names.contains(&"entry/instrument/performance/timestamp".to_string()),
            "performance dataset must be under the layout group; got {:?}",
            names
        );

        drop(h5);
        std::fs::remove_file(&path).ok();
        std::fs::remove_file(&layout).ok();
    }

    /// Layout XML declaring two `<dataset source="detector">` nodes plus a
    /// `<global name="detector_data_destination">`. C `NDFileHDF5` creates
    /// every detector dataset up front (`detDataMap`) and routes each frame to
    /// the one named by the destination NDAttribute, defaulting to the
    /// `det_default` dataset for an absent or unknown value
    /// (NDFileHDF5.cpp:1449-1519). Frames must land in the right dataset and
    /// each dataset must extend to exactly the count it received.
    #[test]
    fn test_detector_data_destination_routes_by_attribute() {
        let dir = std::env::temp_dir();
        let layout = dir.join(format!("adcore_layout_multidet_{}.xml", std::process::id()));
        std::fs::write(
            &layout,
            r#"<hdf5_layout>
              <global name="detector_data_destination" ndattribute="dest"/>
              <group name="entry">
                <dataset name="data1" source="detector" det_default="true"/>
                <dataset name="data2" source="detector"/>
              </group>
            </hdf5_layout>"#,
        )
        .unwrap();

        let path = temp_path("hdf5_multidet_route");
        let mut writer = Hdf5Writer::new();
        writer.set_num_capture(0);
        // Focus the test on image routing; no attribute time-series datasets.
        writer.set_store_attributes(false);
        writer.set_layout_filename(layout.to_str().unwrap());

        // Each frame is a uniform 2x2 UInt16 whose value identifies it, with an
        // optional `dest` string attribute selecting the destination dataset.
        let mk = |val: u16, dest: Option<&str>| {
            let mut arr = NDArray::new(
                vec![NDDimension::new(2), NDDimension::new(2)],
                NDDataType::UInt16,
            );
            if let NDDataViewMut::U16(v) = arr.data_mut().view_mut() {
                for p in v.iter_mut() {
                    *p = val;
                }
            }
            if let Some(d) = dest {
                arr.attributes.add(NDAttribute::new_static(
                    "dest",
                    "",
                    NDAttrSource::Driver,
                    NDAttrValue::String(d.to_string()),
                ));
            }
            arr
        };

        // f0: no dest        -> default data1
        // f1: /entry/data2   -> data2
        // f2: /entry/data2   -> data2
        // f3: /nonexistent   -> unknown, falls back to default data1
        // f4: /entry/data1   -> explicit default data1
        let f0 = mk(10, None);
        writer
            .open_file(&path, NDFileOpenMode::WRITE | NDFileOpenMode::MULTIPLE, &f0)
            .unwrap();
        writer.write_file(&Arc::new(f0)).unwrap();
        writer
            .write_file(&Arc::new(mk(11, Some("/entry/data2"))))
            .unwrap();
        writer
            .write_file(&Arc::new(mk(12, Some("/entry/data2"))))
            .unwrap();
        writer
            .write_file(&Arc::new(mk(13, Some("/nonexistent"))))
            .unwrap();
        writer
            .write_file(&Arc::new(mk(14, Some("/entry/data1"))))
            .unwrap();
        writer.close_file().unwrap();

        let h5 = H5File::open(&path).unwrap();
        let names = h5.dataset_names();
        assert!(
            names.contains(&"entry/data1".to_string())
                && names.contains(&"entry/data2".to_string()),
            "both detector datasets must exist; got {:?}",
            names
        );

        // data1 received f0, f3, f4 (in write order); data2 received f1, f2.
        let d1 = h5.dataset("entry/data1").unwrap();
        assert_eq!(d1.shape(), vec![3, 2, 2], "default dataset extent");
        let v1: Vec<u16> = d1.read_raw().unwrap();
        assert_eq!(
            v1,
            vec![10, 10, 10, 10, 13, 13, 13, 13, 14, 14, 14, 14],
            "default dataset must hold the default-routed frames in order"
        );

        let d2 = h5.dataset("entry/data2").unwrap();
        assert_eq!(d2.shape(), vec![2, 2, 2], "routed dataset extent");
        let v2: Vec<u16> = d2.read_raw().unwrap();
        assert_eq!(
            v2,
            vec![11, 11, 11, 11, 12, 12, 12, 12],
            "routed dataset must hold only the frames addressed to it"
        );

        drop(h5);
        std::fs::remove_file(&path).ok();
        std::fs::remove_file(&layout).ok();
    }

    /// A present-but-non-string `detector_data_destination` attribute aborts the
    /// write, matching C `getValue(NDAttrString,…)` returning `ND_ERROR` →
    /// `asynError` (NDFileHDF5.cpp:1465-1471).
    #[test]
    fn test_detector_data_destination_non_string_attribute_errors() {
        let dir = std::env::temp_dir();
        let layout = dir.join(format!(
            "adcore_layout_multidet_err_{}.xml",
            std::process::id()
        ));
        std::fs::write(
            &layout,
            r#"<hdf5_layout>
              <global name="detector_data_destination" ndattribute="dest"/>
              <group name="entry">
                <dataset name="data1" source="detector" det_default="true"/>
                <dataset name="data2" source="detector"/>
              </group>
            </hdf5_layout>"#,
        )
        .unwrap();

        let path = temp_path("hdf5_multidet_err");
        let mut writer = Hdf5Writer::new();
        writer.set_store_attributes(false);
        writer.set_layout_filename(layout.to_str().unwrap());

        let mut arr = NDArray::new(
            vec![NDDimension::new(2), NDDimension::new(2)],
            NDDataType::UInt16,
        );
        arr.attributes.add(NDAttribute::new_static(
            "dest",
            "",
            NDAttrSource::Driver,
            NDAttrValue::Float64(2.0),
        ));

        writer
            .open_file(
                &path,
                NDFileOpenMode::WRITE | NDFileOpenMode::MULTIPLE,
                &arr,
            )
            .unwrap();
        assert!(
            writer.write_file(&Arc::new(arr)).is_err(),
            "a non-string destination attribute must abort the write"
        );
        writer.close_file().ok();

        std::fs::remove_file(&path).ok();
        std::fs::remove_file(&layout).ok();
    }

    #[test]
    fn test_layout_hardlink_is_materialised() {
        // Regression for BUG 2: a `<hardlink>` declared in the layout XML must
        // produce a real HDF5 hard link in the written file. C ADCore
        // `NDFileHDF5::createHardLinks` walks the layout and calls
        // `H5Lcreate_hard`; without that, files written from a layout with a
        // `<hardlink>` silently lack the link.
        let dir = std::env::temp_dir();
        let layout = dir.join(format!("adcore_layout_hardlink_{}.xml", std::process::id()));
        std::fs::write(
            &layout,
            r#"<hdf5_layout>
              <group name="entry">
                <group name="data">
                  <dataset name="data" source="detector" det_default="true"/>
                  <hardlink name="data_alias" target="/entry/data/data"/>
                </group>
              </group>
            </hdf5_layout>"#,
        )
        .unwrap();

        let path = temp_path("hdf5_layout_hardlink");
        let mut writer = Hdf5Writer::new();
        writer.set_layout_filename(layout.to_str().unwrap());

        let arr = NDArray::new(
            vec![NDDimension::new(4), NDDimension::new(4)],
            NDDataType::UInt16,
        );
        writer
            .open_file(
                &path,
                NDFileOpenMode::WRITE | NDFileOpenMode::MULTIPLE,
                &arr,
            )
            .unwrap();
        writer.write_file(&Arc::new(arr)).unwrap();
        writer.close_file().unwrap();

        let h5 = H5File::open(&path).unwrap();
        let names = h5.dataset_names();
        // The primary dataset at its layout path.
        assert!(
            names.contains(&"entry/data/data".to_string()),
            "image dataset must exist at the layout path; got {:?}",
            names
        );
        // The hard link is an additional name resolving to the same object.
        assert!(
            names.contains(&"entry/data/data_alias".to_string()),
            "layout <hardlink> must be materialised as a hard link; got {:?}",
            names
        );
        // The link shares the target object: same shape, readable as a dataset.
        let alias = h5.dataset("entry/data/data_alias").unwrap();
        let orig = h5.dataset("entry/data/data").unwrap();
        assert_eq!(alias.shape(), orig.shape());

        drop(h5);
        std::fs::remove_file(&path).ok();
        std::fs::remove_file(&layout).ok();
    }

    /// D2: a close whose finalisation fails partway must still release the H5
    /// file id and its dataset ids. The broken `<hardlink>` stands in for the
    /// ENOSPC flush failure: it fails `build_layout_hardlinks`, one of the five
    /// fallible steps that used to sit ahead of `handle.take()`.
    #[test]
    fn close_file_releases_the_handle_when_finalisation_fails() {
        let dir = std::env::temp_dir();
        let layout = dir.join(format!(
            "adcore_layout_broken_hardlink_{}.xml",
            std::process::id()
        ));
        std::fs::write(
            &layout,
            r#"<hdf5_layout>
              <group name="entry">
                <group name="data">
                  <dataset name="data" source="detector" det_default="true"/>
                  <hardlink name="broken" target="/no_such_object"/>
                </group>
              </group>
            </hdf5_layout>"#,
        )
        .unwrap();

        let path = temp_path("hdf5_close_fail_releases_handle");
        let mut writer = Hdf5Writer::new();
        writer.set_layout_filename(layout.to_str().unwrap());

        let arr = NDArray::new(
            vec![NDDimension::new(4), NDDimension::new(4)],
            NDDataType::UInt16,
        );
        writer
            .open_file(
                &path,
                NDFileOpenMode::WRITE | NDFileOpenMode::MULTIPLE,
                &arr,
            )
            .unwrap();
        writer.write_file(&Arc::new(arr)).unwrap();

        assert!(
            writer.close_file().is_err(),
            "a hardlink to a missing object must fail the close"
        );
        assert!(
            writer.handle.is_none(),
            "a failed close must still release the H5 file id and its dataset ids"
        );
        assert!(writer.current_path.is_none());
        // The writer is reusable: a second close is a no-op rather than another
        // run at the same failing finalisation.
        writer.close_file().unwrap();

        std::fs::remove_file(&path).ok();
        std::fs::remove_file(&layout).ok();
    }

    #[test]
    fn test_swmr_layout_hardlink_is_materialised() {
        // A `<hardlink>` declared in the layout XML must also be materialised
        // for SWMR-mode files. C ADCore `NDFileHDF5.cpp:319`-`325` calls
        // `createHardLinks` before `startSWMR()`, so the link is committed by
        // `start_swmr()` and visible to SWMR readers for the whole streaming
        // window. The rust-hdf5 0.2.17 `SwmrFileWriter::create_hard_link` API
        // is called from `open_swmr` before `start_swmr()` — no close-path
        // re-open pass.
        //
        // SWMR mode now places the image dataset at the layout's nested
        // `det_default` path (`/entry/data/data`), exactly like standard mode;
        // the layout hardlink targets that nested path.
        let dir = std::env::temp_dir();
        let layout = dir.join(format!(
            "adcore_swmr_layout_hardlink_{}.xml",
            std::process::id()
        ));
        std::fs::write(
            &layout,
            r#"<hdf5_layout>
              <group name="entry">
                <group name="data">
                  <dataset name="data" source="detector" det_default="true"/>
                  <hardlink name="data_alias" target="/entry/data/data"/>
                </group>
              </group>
            </hdf5_layout>"#,
        )
        .unwrap();

        let path = temp_path("hdf5_swmr_layout_hardlink");
        let mut writer = Hdf5Writer::new();
        writer.set_swmr_mode(true);
        writer.set_num_capture(0);
        writer.set_layout_filename(layout.to_str().unwrap());

        let mut arr = NDArray::new(
            vec![NDDimension::new(4), NDDimension::new(4)],
            NDDataType::UInt16,
        );
        if let NDDataViewMut::U16(v) = arr.data_mut().view_mut() {
            for (i, x) in v.iter_mut().enumerate() {
                *x = i as u16;
            }
        }
        writer
            .open_file(
                &path,
                NDFileOpenMode::WRITE | NDFileOpenMode::MULTIPLE,
                &arr,
            )
            .unwrap();
        assert!(
            writer.is_swmr_active(),
            "writer must be in SWMR mode for this test"
        );
        writer.write_file(&Arc::new(arr.clone())).unwrap();
        writer.write_file(&Arc::new(arr)).unwrap();
        writer.close_file().unwrap();

        let h5 = H5File::open(&path).unwrap();
        let names = h5.dataset_names();
        // The primary SWMR dataset at its nested layout path.
        assert!(
            names.contains(&"entry/data/data".to_string()),
            "SWMR image dataset must exist at the nested layout path; got {:?}",
            names
        );
        // The hard link materialised under the layout group.
        assert!(
            names.contains(&"entry/data/data_alias".to_string()),
            "SWMR layout <hardlink> must be materialised as a hard link; got {:?}",
            names
        );
        // The link shares the target object: same shape, readable as a dataset.
        let alias = h5.dataset("entry/data/data_alias").unwrap();
        let orig = h5.dataset("entry/data/data").unwrap();
        assert_eq!(alias.shape(), orig.shape());
        assert_eq!(orig.shape(), vec![2, 4, 4]);

        drop(h5);
        std::fs::remove_file(&path).ok();
        std::fs::remove_file(&layout).ok();
    }

    #[test]
    fn test_swmr_layout_nested_dataset_placement() {
        // SWMR mode must place the image dataset at the layout's nested
        // `det_default` path — mirroring C `NDFileHDF5` createTree
        // (`NDFileHDF5.cpp:638`) which builds the group tree and creates the
        // detector dataset inside it. The nested dataset, the layout
        // `<hardlink>`, and a constant dataset attribute must all be visible
        // to a `SwmrFileReader` reading the file back.
        let dir = std::env::temp_dir();
        let layout = dir.join(format!(
            "adcore_swmr_layout_nested_{}.xml",
            std::process::id()
        ));
        std::fs::write(
            &layout,
            r#"<hdf5_layout>
              <group name="entry">
                <group name="instrument">
                  <group name="detector">
                    <dataset name="data" source="detector" det_default="true">
                      <attribute name="signal" source="constant" value="1" type="int"/>
                    </dataset>
                    <hardlink name="data_alias" target="/entry/instrument/detector/data"/>
                  </group>
                </group>
                <group name="empty_placeholder"/>
              </group>
            </hdf5_layout>"#,
        )
        .unwrap();

        let path = temp_path("hdf5_swmr_layout_nested");
        let mut writer = Hdf5Writer::new();
        writer.set_swmr_mode(true);
        writer.set_num_capture(0);
        writer.set_layout_filename(layout.to_str().unwrap());

        let mut arr = NDArray::new(
            vec![NDDimension::new(4), NDDimension::new(4)],
            NDDataType::UInt16,
        );
        if let NDDataViewMut::U16(v) = arr.data_mut().view_mut() {
            for (i, x) in v.iter_mut().enumerate() {
                *x = (i * 3) as u16;
            }
        }
        writer
            .open_file(
                &path,
                NDFileOpenMode::WRITE | NDFileOpenMode::MULTIPLE,
                &arr,
            )
            .unwrap();
        assert!(
            writer.is_swmr_active(),
            "writer must be in SWMR mode for this test"
        );
        writer.write_file(&Arc::new(arr.clone())).unwrap();
        writer.write_file(&Arc::new(arr)).unwrap();
        writer.close_file().unwrap();

        // Read back via the SWMR reader — these are the exact paths a live
        // reader attaching during the streaming window would resolve.
        let mut reader = rust_hdf5::swmr::SwmrFileReader::open(&path).unwrap();
        let names = reader.dataset_names();
        // Image dataset at the nested layout path, NOT flat `data`.
        assert!(
            names.contains(&"entry/instrument/detector/data".to_string()),
            "SWMR image dataset must live at the nested layout path; got {:?}",
            names
        );
        assert!(
            !names.contains(&"data".to_string()),
            "SWMR image dataset must NOT remain at the flat root; got {:?}",
            names
        );
        // The empty placeholder group exists.
        assert!(
            reader.has_group("entry/empty_placeholder"),
            "empty layout group must be materialised; groups {:?}",
            reader.group_paths()
        );
        // The layout `<hardlink>` resolves to the nested dataset.
        assert!(
            names.contains(&"entry/instrument/detector/data_alias".to_string()),
            "SWMR layout <hardlink> must resolve to the nested dataset; got {:?}",
            names
        );
        let nested = reader
            .dataset_shape("entry/instrument/detector/data")
            .unwrap();
        let alias = reader
            .dataset_shape("entry/instrument/detector/data_alias")
            .unwrap();
        assert_eq!(nested, vec![2, 4, 4]);
        assert_eq!(alias, nested, "hardlink alias must share the target shape");
        // The data round-trips through both names.
        let via_nested: Vec<u16> = reader
            .read_dataset("entry/instrument/detector/data")
            .unwrap();
        let via_alias: Vec<u16> = reader
            .read_dataset("entry/instrument/detector/data_alias")
            .unwrap();
        assert_eq!(via_nested, via_alias);
        assert_eq!(via_nested.len(), 2 * 4 * 4);
        // The constant layout dataset attribute and the C-parity NDArray default
        // attributes (writeDefaultDatasetAttributes, NDFileHDF5.cpp:3695-3719) all
        // materialised before start_swmr().
        let attr_names = reader
            .dataset_attr_names("entry/instrument/detector/data")
            .unwrap();
        for expected in [
            "signal",
            "NDArrayNumDims",
            "NDArrayDimOffset",
            "NDArrayDimBinning",
            "NDArrayDimReverse",
        ] {
            assert!(
                attr_names.iter().any(|n| n == expected),
                "streaming dataset must carry the {expected} attribute; got {attr_names:?}",
            );
        }

        drop(reader);
        std::fs::remove_file(&path).ok();
        std::fs::remove_file(&layout).ok();
    }

    #[test]
    fn test_default_layout_parses_and_resolves_nexus_paths() {
        // Guards DEFAULT_LAYOUT_XML + the `default_layout()` expect(): the
        // built-in layout must parse and resolve C's NeXus placements
        // (NDFileHDF5LayoutXML.cpp:43-70).
        let layout = Hdf5Writer::default_layout();
        assert_eq!(
            layout.detector_dataset_path().as_deref(),
            Some("/entry/instrument/detector/data")
        );
        assert_eq!(
            layout.ndattr_default_group().as_deref(),
            Some("/entry/instrument/NDAttributes")
        );
        assert_eq!(
            layout.dataset_group_path("timestamp").as_deref(),
            Some("/entry/instrument/performance")
        );
    }

    #[test]
    fn test_no_layout_uses_default_nexus_layout() {
        // With no layout file the writer loads C's built-in DEFAULT_LAYOUT
        // (NDFileHDF5LayoutXML.cpp:43-70): the detector image lands at
        // /entry/instrument/detector/data inside the NeXus tree, with an
        // /entry/data/data hardlink to it — not a flat-root `data`.
        let path = temp_path("hdf5_default_layout");
        let mut writer = Hdf5Writer::new();
        let arr = NDArray::new(
            vec![NDDimension::new(4), NDDimension::new(4)],
            NDDataType::UInt8,
        );
        writer
            .open_file(&path, NDFileOpenMode::WRITE, &arr)
            .unwrap();
        writer.write_file(&Arc::new(arr)).unwrap();
        writer.close_file().unwrap();

        let h5 = H5File::open(&path).unwrap();
        // Detector dataset at the NeXus path, not flat root.
        let det = h5.dataset("entry/instrument/detector/data").unwrap();
        assert_eq!(det.shape(), vec![4, 4]);
        assert!(
            !h5.dataset_names().contains(&"data".to_string()),
            "must not write a flat-root `data`; got {:?}",
            h5.dataset_names()
        );
        // The NXdata hardlink /entry/data/data resolves to the same image.
        let linked = h5.dataset("entry/data/data").unwrap();
        assert_eq!(linked.shape(), vec![4, 4]);
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn test_default_layout_group_nx_class_attributes() {
        // C attaches NX_class markers to the default-layout NeXus groups via
        // writeHdfAttributes (NDFileHDF5.cpp:693-695): NXentry/NXinstrument/
        // NXdetector/NXcollection/NXdata.
        let path = temp_path("hdf5_nxclass");
        let mut writer = Hdf5Writer::new();
        let arr = NDArray::new(
            vec![NDDimension::new(4), NDDimension::new(4)],
            NDDataType::UInt8,
        );
        writer
            .open_file(&path, NDFileOpenMode::WRITE, &arr)
            .unwrap();
        writer.write_file(&Arc::new(arr)).unwrap();
        writer.close_file().unwrap();

        let h5 = H5File::open(&path).unwrap();
        let nx = |g: &str| {
            let mut grp = h5.root_group();
            for seg in g.split('/') {
                grp = grp.group(seg).unwrap();
            }
            grp.attr_string("NX_class").unwrap()
        };
        assert_eq!(nx("entry"), "NXentry");
        assert_eq!(nx("entry/instrument"), "NXinstrument");
        assert_eq!(nx("entry/instrument/detector"), "NXdetector");
        assert_eq!(nx("entry/instrument/NDAttributes"), "NXcollection");
        assert_eq!(nx("entry/instrument/detector/NDAttributes"), "NXcollection");
        assert_eq!(nx("entry/data"), "NXdata");
        std::fs::remove_file(&path).ok();
    }

    /// Each `openFile` creates a new file (NDFileHDF5.cpp:271), so the
    /// attribute names created in the last file do not carry over.
    fn assert_second_file_gets_every_attribute(swmr: bool) {
        let mut writer = Hdf5Writer::new();
        writer.set_swmr_mode(swmr);
        let arr = NDArray::new(
            vec![NDDimension::new(4), NDDimension::new(4)],
            NDDataType::UInt8,
        );
        let mut names = Vec::new();
        for n in 0..2 {
            let path = temp_path(&format!("hdf5_two_files_{swmr}_{n}"));
            writer
                .open_file(
                    &path,
                    NDFileOpenMode::WRITE | NDFileOpenMode::MULTIPLE,
                    &arr,
                )
                .unwrap();
            writer.write_file(&Arc::new(arr.clone())).unwrap();
            writer.close_file().unwrap();
            let h5 = H5File::open(&path).unwrap();
            let group = |g: &str| {
                let mut grp = h5.root_group();
                for seg in g.split('/') {
                    grp = grp.group(seg).unwrap();
                }
                grp.attr_names().unwrap()
            };
            names.push([
                group("entry"),
                group("entry/instrument/NDAttributes"),
                group("entry/data"),
                h5.dataset("entry/instrument/detector/data")
                    .unwrap()
                    .attr_names()
                    .unwrap(),
            ]);
            std::fs::remove_file(&path).ok();
        }
        assert!(names[0].iter().all(|n| !n.is_empty()), "{swmr}");
        assert_eq!(names[1], names[0], "{swmr}");
    }

    #[test]
    fn a_second_file_gets_every_attribute() {
        assert_second_file_gets_every_attribute(false);
    }

    #[test]
    fn a_second_swmr_file_gets_every_attribute() {
        assert_second_file_gets_every_attribute(true);
    }

    #[test]
    fn test_swmr_default_layout_group_nx_class_attributes() {
        // The SWMR path materialises the same NX_class group markers
        // (build_swmr_layout_groups → write_swmr_group_constant_attrs).
        let path = temp_path("hdf5_swmr_nxclass");
        let mut writer = Hdf5Writer::new();
        writer.set_swmr_mode(true);
        let arr = NDArray::new(
            vec![NDDimension::new(4), NDDimension::new(4)],
            NDDataType::Float32,
        );
        writer
            .open_file(
                &path,
                NDFileOpenMode::WRITE | NDFileOpenMode::MULTIPLE,
                &arr,
            )
            .unwrap();
        writer.write_file(&Arc::new(arr)).unwrap();
        writer.close_file().unwrap();

        let h5 = H5File::open(&path).unwrap();
        let nx = |g: &str| {
            let mut grp = h5.root_group();
            for seg in g.split('/') {
                grp = grp.group(seg).unwrap();
            }
            grp.attr_string("NX_class").unwrap()
        };
        assert_eq!(nx("entry"), "NXentry");
        assert_eq!(nx("entry/instrument/detector"), "NXdetector");
        assert_eq!(nx("entry/data"), "NXdata");
        std::fs::remove_file(&path).ok();
    }

    /// writeH5attrInt32 / writeH5attrFloat64 (NDFileHDF5.cpp:1117-1285) store a
    /// layout constant as NATIVE_INT32 / NATIVE_DOUBLE, read as a comma list.
    fn assert_layout_constants_typed_as_c(swmr: bool) {
        let dir = std::env::temp_dir();
        let layout = dir.join(format!(
            "adcore_layout_consts_{}_{swmr}.xml",
            std::process::id()
        ));
        std::fs::write(
            &layout,
            r#"<hdf5_layout>
              <group name="entry">
                <attribute name="gi3" source="constant" value="1,2, 3" type="int"/>
                <attribute name="gf2" source="constant" value="1.5,-2e1" type="float"/>
                <dataset name="data" source="detector" det_default="true">
                  <attribute name="i1" source="constant" value="7" type="int"/>
                  <attribute name="i3" source="constant" value="1,2, 3" type="int"/>
                  <attribute name="f1" source="constant" value="2.5" type="float"/>
                  <attribute name="f2" source="constant" value="1.5,-2e1" type="float"/>
                </dataset>
                <group name="NDAttributes" ndattr_default="true"/>
              </group>
            </hdf5_layout>"#,
        )
        .unwrap();
        let path = temp_path("hdf5_layout_consts");
        let mut writer = Hdf5Writer::new();
        writer.set_swmr_mode(swmr);
        writer.set_layout_filename(layout.to_str().unwrap());
        let arr = NDArray::new(
            vec![NDDimension::new(4), NDDimension::new(4)],
            NDDataType::UInt8,
        );
        writer
            .open_file(
                &path,
                NDFileOpenMode::WRITE | NDFileOpenMode::MULTIPLE,
                &arr,
            )
            .unwrap();
        writer.write_file(&Arc::new(arr)).unwrap();
        writer.close_file().unwrap();

        let h5 = H5File::open(&path).unwrap();
        let ds = h5.dataset("entry/data").unwrap();
        let int = |name: &str| {
            let a = ds.attr(name).unwrap();
            (a.datatype().unwrap(), a.read_numeric_as::<i64>().unwrap())
        };
        let float = |name: &str| {
            let a = ds.attr(name).unwrap();
            (a.datatype().unwrap(), a.read_numeric_as::<f64>().unwrap())
        };
        let (i32t, f64t) = (
            <i32 as rust_hdf5::types::H5Type>::hdf5_type(),
            <f64 as rust_hdf5::types::H5Type>::hdf5_type(),
        );
        assert_eq!(int("i1"), (i32t.clone(), vec![7]), "swmr={swmr}");
        assert_eq!(int("i3"), (i32t, vec![1, 2, 3]), "swmr={swmr}");
        assert_eq!(float("f1"), (f64t.clone(), vec![2.5]), "swmr={swmr}");
        assert_eq!(float("f2"), (f64t, vec![1.5, -20.0]), "swmr={swmr}");
        // rust-hdf5 reads no numeric group attribute; h5py has the values.
        let entry = h5.root_group().group("entry").unwrap();
        let names = entry.attr_names().unwrap();
        for name in ["gi3", "gf2"] {
            assert!(names.iter().any(|n| n == name), "swmr={swmr} {name}");
        }
        std::fs::remove_file(&path).ok();
        std::fs::remove_file(&layout).ok();
    }

    #[test]
    fn layout_constants_read_as_cpp_stream_extraction() {
        let ints = |s: &str| stream_numbers(s, read_int);
        let doubles = |s: &str| stream_numbers(s, read_double);
        assert_eq!(ints("1,2, 3"), [1, 2, 3]);
        assert_eq!(ints(" 1 2\t3"), [1, 2, 3]);
        assert_eq!(ints("1 ,2"), [1]);
        assert_eq!(ints("5 V"), [5]);
        assert_eq!(ints("1.5"), [1]);
        assert_eq!(ints("+4,-4"), [4, -4]);
        assert_eq!(ints("2147483648"), Vec::<i32>::new());
        assert_eq!(ints(""), Vec::<i32>::new());
        assert_eq!(ints("-"), Vec::<i32>::new());
        assert_eq!(doubles(".5,-2e1,1.e2,3."), [0.5, -20.0, 100.0, 3.0]);
        assert_eq!(doubles("1e"), Vec::<f64>::new());
        assert_eq!(doubles("1e999"), Vec::<f64>::new());
        assert_eq!(doubles("."), Vec::<f64>::new());
        assert_eq!(doubles("2,x"), [2.0]);
    }

    #[test]
    fn layout_constant_attributes_are_int32_and_double_lists() {
        assert_layout_constants_typed_as_c(false);
    }

    #[test]
    fn swmr_layout_constant_attributes_are_int32_and_double_lists() {
        assert_layout_constants_typed_as_c(true);
    }

    /// Element::add_attribute keeps an element's first `<attribute>` of a name,
    /// and writeDefaultDatasetAttributes adds the NDArray geometry through it
    /// after the layout's own (NDFileHDF5Layout.cpp:251-259,
    /// NDFileHDF5.cpp:3740-3750): the C oracle file holds `first`, `sig=1`,
    /// the layout's NDArrayNumDims and the NDAttribute under NDArrayDimOffset.
    #[test]
    fn a_layout_element_keeps_its_first_attribute_of_a_name() {
        for swmr in [false, true] {
            let path = temp_path("hdf5_first_attr");
            let mut writer = Hdf5Writer::new();
            writer.set_swmr_mode(swmr);
            writer.set_layout_filename(
                r#"<?xml version="1.0"?><hdf5_layout><group name="entry">
                  <attribute name="dup" source="constant" value="first" type="string"/>
                  <attribute name="dup" source="constant" value="2" type="int"/>
                  <dataset name="data" source="detector" det_default="true">
                    <attribute name="sig" source="constant" value="1" type="int"/>
                    <attribute name="sig" source="constant" value="second" type="string"/>
                    <attribute name="NDArrayNumDims" source="constant" value="9" type="int"/>
                    <attribute name="NDArrayDimOffset" source="ndattribute" ndattribute="AcqT" when="OnFileOpen"/>
                  </dataset>
                  <group name="NDAttributes" ndattr_default="true"/>
                </group></hdf5_layout>"#,
            );
            let mut arr = NDArray::new(
                vec![NDDimension::new(4), NDDimension::new(4)],
                NDDataType::UInt8,
            );
            arr.attributes.add(NDAttribute::new_static(
                "AcqT",
                "",
                NDAttrSource::Driver,
                NDAttrValue::Float64(0.25),
            ));
            writer
                .open_file(&path, NDFileOpenMode::WRITE, &arr)
                .unwrap();
            writer.write_file(&Arc::new(arr)).unwrap();
            writer.close_file().unwrap();

            let h5 = H5File::open(&path).unwrap();
            let entry = h5.root_group().group("entry").unwrap();
            assert_eq!(entry.attr_string("dup").unwrap(), "first", "swmr={swmr}");
            let ds = h5.dataset("entry/data").unwrap();
            let int = |name: &str| ds.attr(name).unwrap().read_numeric_as::<i64>().unwrap();
            assert_eq!(int("sig"), [1], "swmr={swmr}");
            assert_eq!(int("NDArrayNumDims"), [9], "swmr={swmr}");
            let offset = ds.attr("NDArrayDimOffset").unwrap();
            assert_eq!(
                offset.read_numeric_as::<f64>().unwrap(),
                [0.25],
                "swmr={swmr}"
            );
            std::fs::remove_file(&path).ok();
        }
    }

    /// Write a file through a layout holding `<dataset source="constant">`
    /// nodes in `entry`, returning its path.
    fn write_with_constant_datasets(swmr: bool) -> PathBuf {
        let layout = std::env::temp_dir().join(format!(
            "adcore_layout_const_dsets_{}_{swmr}.xml",
            std::process::id()
        ));
        std::fs::write(
            &layout,
            r#"<hdf5_layout>
              <group name="entry">
                <dataset name="data" source="detector" det_default="true"/>
                <dataset name="name" source="constant" value="MyBeamline" type="string"/>
                <dataset name="untyped" source="constant" value="dropped"/>
                <dataset name="empty" source="constant" value="" type="int"/>
                <dataset name="i1" source="constant" value="7" type="int"/>
                <dataset name="i3" source="constant" value="1,2,3" type="int"/>
                <dataset name="f1" source="constant" value="2.5" type="float"/>
                <dataset name="f2" source="constant" value="1.5,-2" type="float"/>
                <group name="NDAttributes" ndattr_default="true"/>
              </group>
            </hdf5_layout>"#,
        )
        .unwrap();
        let path = temp_path("hdf5_const_dsets");
        let mut writer = Hdf5Writer::new();
        writer.set_swmr_mode(swmr);
        writer.set_layout_filename(layout.to_str().unwrap());
        let arr = NDArray::new(
            vec![NDDimension::new(4), NDDimension::new(4)],
            NDDataType::UInt8,
        );
        writer
            .open_file(
                &path,
                NDFileOpenMode::WRITE | NDFileOpenMode::MULTIPLE,
                &arr,
            )
            .unwrap();
        writer.write_file(&Arc::new(arr)).unwrap();
        writer.close_file().unwrap();
        std::fs::remove_file(&layout).ok();
        path
    }

    /// writeH5dsetInt32 / writeH5dsetFloat64 (NDFileHDF5.cpp:857-1035): a
    /// contiguous int32 or double, scalar for one value and 1-D otherwise,
    /// so a value that reads no number is a 1-D dataset of length 0.
    fn assert_numeric_constant_datasets(h5: &H5File) {
        use rust_hdf5::StorageLayout;
        let check = |name: &str, dt: DatatypeMessage, shape: Vec<usize>| {
            let ds = h5.dataset(&format!("entry/{name}")).unwrap();
            assert_eq!(ds.datatype().unwrap(), dt, "{name}");
            assert_eq!(ds.shape(), shape, "{name}");
            assert_eq!(
                ds.storage_layout().unwrap(),
                StorageLayout::Contiguous,
                "{name}"
            );
            ds
        };
        let i32t = <i32 as rust_hdf5::types::H5Type>::hdf5_type();
        let f64t = <f64 as rust_hdf5::types::H5Type>::hdf5_type();
        assert_eq!(
            check("i1", i32t.clone(), vec![]).read_raw::<i32>().unwrap(),
            [7]
        );
        assert_eq!(
            check("i3", i32t.clone(), vec![3])
                .read_raw::<i32>()
                .unwrap(),
            [1, 2, 3]
        );
        assert!(
            check("empty", i32t, vec![0])
                .read_raw::<i32>()
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            check("f1", f64t.clone(), vec![]).read_raw::<f64>().unwrap(),
            [2.5]
        );
        assert_eq!(
            check("f2", f64t, vec![2]).read_raw::<f64>().unwrap(),
            [1.5, -2.0]
        );
    }

    /// writeH5dsetStr (NDFileHDF5.cpp:826-855): a 1-D length-1 C_S1 of
    /// `len + 1` NULLTERM bytes. A constant dataset without `type` keeps no
    /// value (NDFileHDF5LayoutXML.cpp:583-592), so it is the empty string.
    fn assert_layout_constant_datasets_are_written_as_c(swmr: bool) {
        let path = write_with_constant_datasets(swmr);
        let h5 = H5File::open(&path).unwrap();
        assert_numeric_constant_datasets(&h5);
        for (name, text) in [("name", "MyBeamline"), ("untyped", "")] {
            let ds = h5.dataset(&format!("entry/{name}")).unwrap();
            let mut bytes = text.as_bytes().to_vec();
            bytes.push(0);
            assert_eq!(
                ds.datatype().unwrap(),
                DatatypeMessage::FixedString {
                    size: bytes.len() as u32,
                    padding: 0,
                    charset: 0
                },
                "{name} swmr={swmr}"
            );
            assert_eq!(ds.shape(), vec![1], "{name}");
            assert_eq!(ds.read_raw_bytes().unwrap(), bytes, "{name}");
        }
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn layout_constant_datasets_are_written_as_c() {
        assert_layout_constant_datasets_are_written_as_c(false);
    }

    #[test]
    fn swmr_layout_constant_datasets_are_written_as_c() {
        assert_layout_constant_datasets_are_written_as_c(true);
    }

    /// Write a frame through a layout with constant and ndattribute
    /// `<attribute>` nodes on groups and on the detector dataset.
    fn write_with_store_attr(swmr: bool, store: bool) -> PathBuf {
        let layout = std::env::temp_dir().join(format!(
            "adcore_layout_store_attr_{}_{swmr}_{store}.xml",
            std::process::id()
        ));
        std::fs::write(
            &layout,
            r#"<hdf5_layout>
              <group name="entry">
                <attribute name="NX_class" source="constant" value="NXentry" type="string"/>
                <attribute name="GainAtOpen" source="ndattribute" ndattribute="Gain" when="OnFileOpen"/>
                <dataset name="data" source="detector" det_default="true">
                  <attribute name="units" source="constant" value="counts" type="string"/>
                  <attribute name="GainAtOpen" source="ndattribute" ndattribute="Gain" when="OnFileOpen"/>
                </dataset>
                <group name="NDAttributes" ndattr_default="true">
                  <attribute name="NX_class" source="constant" value="NXcollection" type="string"/>
                </group>
              </group>
            </hdf5_layout>"#,
        )
        .unwrap();
        let path = temp_path("hdf5_store_attr");
        let mut writer = Hdf5Writer::new();
        writer.set_swmr_mode(swmr);
        writer.set_store_attributes(store);
        writer.set_layout_filename(layout.to_str().unwrap());
        let mut arr = NDArray::new(
            vec![NDDimension::new(4), NDDimension::new(4)],
            NDDataType::UInt8,
        );
        arr.attributes.add(NDAttribute::new_static(
            "Gain",
            "",
            NDAttrSource::Driver,
            NDAttrValue::Int32(10),
        ));
        writer
            .open_file(&path, NDFileOpenMode::WRITE, &arr)
            .unwrap();
        writer.write_file(&Arc::new(arr)).unwrap();
        writer.close_file().unwrap();
        std::fs::remove_file(&layout).ok();
        path
    }

    /// createTree writes a group's `<attribute>` nodes only when StoreAttr is
    /// 1 (NDFileHDF5.cpp:690-696), and the ndattribute ones on any element
    /// are stored only then (:297-309, :1661-1664); a dataset's constant
    /// `<attribute>` is written either way (:686).
    fn assert_store_attr_gates_element_attrs(swmr: bool) {
        for store in [false, true] {
            let path = write_with_store_attr(swmr, store);
            let h5 = H5File::open(&path).unwrap();
            let sorted = |mut v: Vec<String>| {
                v.sort();
                v
            };
            let entry = h5.root_group().group("entry").unwrap();
            let ndattrs = entry.group("NDAttributes").unwrap();
            let data = h5.dataset("entry/data").unwrap();
            let data_attrs = data.attr_names().unwrap();
            assert!(data_attrs.contains(&"units".to_owned()), "{swmr} {store}");
            if store {
                assert_eq!(
                    sorted(entry.attr_names().unwrap()),
                    ["GainAtOpen", "NX_class"],
                    "{swmr}"
                );
                assert_eq!(
                    sorted(ndattrs.attr_names().unwrap()),
                    ["NX_class", "hostname"],
                    "{swmr}"
                );
                assert!(data_attrs.contains(&"GainAtOpen".to_owned()), "{swmr}");
            } else {
                assert!(entry.attr_names().unwrap().is_empty(), "{swmr}");
                assert!(ndattrs.attr_names().unwrap().is_empty(), "{swmr}");
                assert!(!data_attrs.contains(&"GainAtOpen".to_owned()), "{swmr}");
            }
            std::fs::remove_file(&path).ok();
        }
    }

    #[test]
    fn store_attr_gates_group_and_ndattribute_element_attrs() {
        assert_store_attr_gates_element_attrs(false);
    }

    #[test]
    fn swmr_store_attr_gates_group_and_ndattribute_element_attrs() {
        assert_store_attr_gates_element_attrs(true);
    }

    /// C's layout reader knows only `OnFileOpen` and `OnFileClose`; any other
    /// `when`, `OnFileWrite` included, is `OnFrame` (NDFileHDF5LayoutXML.cpp:
    /// 395-405), which storeOnOpenCloseAttribute writes at open.
    fn assert_on_file_write_element_attrs_are_on_frame(swmr: bool) {
        let layout = std::env::temp_dir().join(format!(
            "adcore_layout_on_file_write_{}_{swmr}.xml",
            std::process::id()
        ));
        std::fs::write(
            &layout,
            r#"<hdf5_layout>
              <group name="entry">
                <attribute name="AW" source="ndattribute" ndattribute="Gain" when="OnFileWrite"/>
                <dataset name="data" source="detector" det_default="true">
                  <attribute name="DW" source="ndattribute" ndattribute="Gain" when="OnFileWrite"/>
                </dataset>
              </group>
            </hdf5_layout>"#,
        )
        .unwrap();
        let path = temp_path("hdf5_on_file_write");
        let mut writer = Hdf5Writer::new();
        writer.set_swmr_mode(swmr);
        writer.set_layout_filename(layout.to_str().unwrap());
        let mut arr = NDArray::new(
            vec![NDDimension::new(4), NDDimension::new(4)],
            NDDataType::UInt8,
        );
        arr.attributes.add(NDAttribute::new_static(
            "Gain",
            "",
            NDAttrSource::Driver,
            NDAttrValue::Int32(10),
        ));
        writer
            .open_file(&path, NDFileOpenMode::WRITE, &arr)
            .unwrap();
        writer.write_file(&Arc::new(arr)).unwrap();
        writer.close_file().unwrap();
        std::fs::remove_file(&layout).ok();

        let aw: Vec<Vec<u8>> = attr_messages(&path, "AW")
            .into_iter()
            .map(|m| m.data)
            .collect();
        assert_eq!(aw, [10i32.to_le_bytes()], "swmr={swmr}");
        let h5 = H5File::open(&path).unwrap();
        let data = h5.dataset("entry/data").unwrap();
        assert_eq!(numeric_attr::<i64>(&data, "DW"), [10], "swmr={swmr}");
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn on_file_write_element_attrs_are_on_frame() {
        assert_on_file_write_element_attrs_are_on_frame(false);
    }

    #[test]
    fn swmr_on_file_write_element_attrs_are_on_frame() {
        assert_on_file_write_element_attrs_are_on_frame(true);
    }

    /// Write a frame through a layout whose constant and NDAttribute datasets
    /// carry constant and ndattribute `<attribute>` nodes.
    fn write_with_dataset_element_attrs(swmr: bool) -> PathBuf {
        let layout = std::env::temp_dir().join(format!(
            "adcore_layout_dset_attrs_{}_{swmr}.xml",
            std::process::id()
        ));
        std::fs::write(
            &layout,
            r#"<hdf5_layout>
              <group name="entry">
                <dataset name="data" source="detector" det_default="true"/>
                <dataset name="name" source="constant" value="MyBeamline" type="string">
                  <attribute name="units" source="constant" value="mm" type="string"/>
                  <attribute name="ai" source="constant" value="3,4" type="int"/>
                  <attribute name="GainAtOpen" source="ndattribute" ndattribute="Gain" when="OnFileOpen"/>
                </dataset>
                <dataset name="i1" source="constant" value="7" type="int">
                  <attribute name="af" source="constant" value="0.5" type="float"/>
                  <attribute name="GainAtOpen" source="ndattribute" ndattribute="Gain" when="OnFileOpen"/>
                </dataset>
                <dataset name="gain" source="ndattribute" ndattribute="Gain">
                  <attribute name="units" source="constant" value="dB" type="string"/>
                  <attribute name="af" source="constant" value="0.5" type="float"/>
                  <attribute name="GainAtOpen" source="ndattribute" ndattribute="Gain" when="OnFileOpen"/>
                </dataset>
                <group name="NDAttributes" ndattr_default="true"/>
              </group>
            </hdf5_layout>"#,
        )
        .unwrap();
        let path = temp_path("hdf5_dset_attrs");
        let mut writer = Hdf5Writer::new();
        writer.set_swmr_mode(swmr);
        writer.set_layout_filename(layout.to_str().unwrap());
        let mut arr = NDArray::new(
            vec![NDDimension::new(4), NDDimension::new(4)],
            NDDataType::UInt8,
        );
        arr.attributes.add(NDAttribute::new_static(
            "Gain",
            "",
            NDAttrSource::Driver,
            NDAttrValue::Int32(10),
        ));
        writer
            .open_file(&path, NDFileOpenMode::WRITE, &arr)
            .unwrap();
        writer.write_file(&Arc::new(arr)).unwrap();
        writer.close_file().unwrap();
        std::fs::remove_file(&layout).ok();
        path
    }

    fn numeric_attr<T: rust_hdf5::dataset::ReadNumeric>(
        ds: &rust_hdf5::H5Dataset,
        name: &str,
    ) -> Vec<T> {
        ds.attr(name).unwrap().read_numeric_as::<T>().unwrap()
    }

    /// createTree writes a constant dataset's `<attribute>` nodes as it does
    /// any other dataset's (NDFileHDF5.cpp:686-687), and createAttributeDataset
    /// an NDAttribute dataset's (:2816).
    fn assert_layout_attributes_reach_constant_and_ndattribute_datasets(swmr: bool) {
        let path = write_with_dataset_element_attrs(swmr);
        let h5 = H5File::open(&path).unwrap();
        let name = h5.dataset("entry/name").unwrap();
        assert_eq!(name.attr("units").unwrap().read_string().unwrap(), "mm");
        assert_eq!(numeric_attr::<i64>(&name, "ai"), [3, 4]);
        assert_eq!(numeric_attr::<i64>(&name, "GainAtOpen"), [10]);
        let i1 = h5.dataset("entry/i1").unwrap();
        assert_eq!(numeric_attr::<f64>(&i1, "af"), [0.5]);
        assert_eq!(numeric_attr::<i64>(&i1, "GainAtOpen"), [10]);
        let gain = h5.dataset("entry/gain").unwrap();
        assert_eq!(gain.read_raw::<i32>().unwrap(), [10]);
        assert_eq!(gain.attr("units").unwrap().read_string().unwrap(), "dB");
        assert_eq!(numeric_attr::<f64>(&gain, "af"), [0.5]);
        assert_eq!(numeric_attr::<i64>(&gain, "GainAtOpen"), [10]);
        assert_eq!(
            gain.attr("NDAttrName").unwrap().read_string().unwrap(),
            "Gain"
        );
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn layout_attributes_reach_constant_and_ndattribute_datasets() {
        assert_layout_attributes_reach_constant_and_ndattribute_datasets(false);
    }

    #[test]
    fn swmr_layout_attributes_reach_constant_and_ndattribute_datasets() {
        assert_layout_attributes_reach_constant_and_ndattribute_datasets(true);
    }

    // ---- ADP-99: direct chunk write of pre-compressed NDArrays -----------

    /// An uncompressed UInt16 frame with a deterministic ramp, ready to feed a
    /// codec. `dims = [y, x]` (HDF5 fastest axis last).
    fn ramp_u16(y: usize, x: usize) -> NDArray {
        let data: Vec<u16> = (0..(y * x) as u16).collect();
        NDArray::with_data(
            vec![NDDimension::new(y), NDDimension::new(x)],
            NDDataBuffer::from_vec::<u16>(data),
        )
    }

    fn u16_pixels(arr: &NDArray) -> Vec<u16> {
        match arr.data().view() {
            NDDataView::U16(v) => v.to_vec(),
            _ => unreachable!("expected U16 buffer"),
        }
    }

    #[test]
    fn test_codec_chunk_bytes_lz4_prepends_hdf5_header() {
        // C NDFileHDF5Dataset::writeFile (NDFileHDF5Dataset.cpp:299-314): a raw
        // LZ4 block gets a 16-byte big-endian header — uncompressed size (u64),
        // block size = uncompressed size (u32), compressed size (u32) — then the
        // block bytes.
        let codec = Codec {
            name: CodecName::LZ4,
            compressed_size: 4,
            level: 0,
            shuffle: 0,
            compressor: 0,
            original_data_type: NDDataType::UInt16,
        };
        let payload = [0xAAu8, 0xBB, 0xCC, 0xDD];
        let out = codec_chunk_bytes(&codec, 32, &payload);
        let mut expect = Vec::new();
        expect.extend_from_slice(&32u64.to_be_bytes()); // uncompressed size
        expect.extend_from_slice(&32u32.to_be_bytes()); // block size
        expect.extend_from_slice(&4u32.to_be_bytes()); // compressed size
        expect.extend_from_slice(&payload);
        assert_eq!(out, expect);
    }

    #[test]
    fn test_codec_chunk_bytes_verbatim_for_self_describing() {
        // BLOSC and JPEG emit self-describing streams, so the chunk is written
        // verbatim — no extra header. (LZ4 and BSLZ4 get a chunk header; see
        // test_direct_chunk_write_lz4_roundtrips / test_codec_chunk_bytes_bslz4_header.)
        for name in [CodecName::Blosc, CodecName::JPEG] {
            let codec = Codec {
                name,
                compressed_size: 3,
                level: 0,
                shuffle: 0,
                compressor: 0,
                original_data_type: NDDataType::UInt8,
            };
            let payload = [1u8, 2, 3];
            assert_eq!(codec_chunk_bytes(&codec, 99, &payload), payload.to_vec());
        }
    }

    #[test]
    fn test_codecs_match() {
        let a = Codec {
            name: CodecName::LZ4,
            compressed_size: 1,
            level: 0,
            shuffle: 0,
            compressor: 0,
            original_data_type: NDDataType::UInt8,
        };
        assert!(codecs_match(None, None));
        assert!(!codecs_match(Some(&a), None));
        assert!(!codecs_match(None, Some(&a)));
        assert!(codecs_match(Some(&a), Some(&a.clone())));
        let mut diff = a.clone();
        diff.compressor = 1;
        assert!(!codecs_match(Some(&a), Some(&diff)));
        // original_data_type is NOT part of codec identity (C Codec_t::operator!=).
        let mut other_type = a.clone();
        other_type.original_data_type = NDDataType::Float64;
        assert!(codecs_match(Some(&a), Some(&other_type)));
    }

    /// An override's -1 blosc params reach the filter as C's unsigned cds.
    #[test]
    fn blosc_cds_take_the_params_as_unsigned() {
        let mut s = Hdf5Settings::default();
        s.set_compression_type(COMPRESS_BLOSC);
        s.set_blosc_compress_level(-1);
        s.set_blosc_shuffle_type(-1);
        s.set_blosc_compressor(-1);
        let p = s.build_pipeline(2, &mono_frame()).unwrap();
        assert_eq!(p.filters[0].cd_values[4..], [u32::MAX; 3]);
    }

    /// C's zlib out of range: `H5Pset_deflate` fails, so the dataset has no
    /// filter while its codec is still zlib at that level (NDFileHDF5.cpp:
    /// 3378-3384).
    #[test]
    fn a_zlib_level_hdf5_refuses_sets_no_filter() {
        let mut s = Hdf5Settings::default();
        s.set_compression_type(COMPRESS_ZLIB);
        for level in [-1, 10] {
            s.set_z_compress_level(level);
            assert!(s.build_pipeline(2, &mono_frame()).is_none(), "{level}");
            let c = s.dataset_codec(NDDataType::UInt16, &mono_frame()).unwrap();
            assert_eq!((c.name, c.level), (CodecName::Zlib, level));
        }
        for level in [0, 9] {
            s.set_z_compress_level(level);
            let p = s.build_pipeline(2, &mono_frame()).unwrap();
            assert_eq!(p.filters[0].cd_values, [level as u32], "{level}");
        }
    }

    /// A 2-D frame with no ColorMode, which C's JPEG takes as Mono.
    fn mono_frame() -> NDArray {
        NDArray::new(
            vec![NDDimension::new(4), NDDimension::new(5)],
            NDDataType::UInt16,
        )
    }

    #[test]
    fn test_frame_codec_overrides_the_scheme() {
        // C configureCompression's override (NDFileHDF5.cpp:3316-3337), the
        // filter each scheme sets and the codec it records (:3352-3469).
        use rust_hdf5::format::messages::filter::FILTER_DEFLATE;
        let s = Hdf5Settings::default();
        let mk = |name| Codec {
            name,
            compressed_size: 0,
            level: 5,
            shuffle: 1,
            compressor: 2,
            original_data_type: NDDataType::UInt16,
        };
        let over = |name| {
            let mut o = s.clone();
            o.apply(&Hdf5Settings::codec_override(Some(&mk(name))));
            o
        };
        let id = |name| {
            over(name)
                .build_pipeline(2, &mono_frame())
                .map(|p| p.filters[0].id)
        };
        assert_eq!(id(CodecName::LZ4), Some(FILTER_LZ4));
        assert_eq!(id(CodecName::LZ4HDF5), Some(FILTER_LZ4));
        assert_eq!(id(CodecName::BSLZ4), Some(FILTER_BSHUF));
        assert_eq!(id(CodecName::JPEG), Some(FILTER_JPEG));
        assert_eq!(id(CodecName::Zlib), Some(FILTER_DEFLATE));
        // No override for a name C does not list: the scheme (None) stays.
        assert_eq!(id(CodecName::from_name("szip")), None);
        let blosc = over(CodecName::Blosc);
        assert_eq!(
            blosc.build_pipeline(2, &mono_frame()).unwrap().filters[0].cd_values[4..],
            [5, 1, 2]
        );
        let codec = |s: Hdf5Settings| {
            let c = s.dataset_codec(NDDataType::UInt16, &mono_frame()).unwrap();
            (c.name, c.level, c.shuffle, c.compressor)
        };
        assert_eq!(codec(blosc), (CodecName::Blosc, 5, 1, 2));
        let zlib = over(CodecName::Zlib);
        assert_eq!(codec(zlib), (CodecName::Zlib, 5, -1, -1));
        let lz4 = over(CodecName::LZ4);
        assert_eq!(codec(lz4), (CodecName::LZ4, -1, -1, -1));
        assert!(s.dataset_codec(NDDataType::UInt16, &mono_frame()).is_none());
    }

    #[test]
    fn test_compression_aware_true() {
        // C NDFileHDF5.cpp:2268 passes compressionAware=true.
        assert!(Hdf5FileProcessor::new().compression_aware());
    }

    #[test]
    fn test_direct_chunk_write_lz4_roundtrips() {
        let path = temp_path("hdf5_dcw_lz4");
        let mut writer = Hdf5Writer::new();
        let orig = ramp_u16(4, 5);
        let expect = u16_pixels(&orig);
        let comp = crate::codec::compress_lz4(&orig).unwrap();
        assert!(comp.codec.is_some());
        writer
            .open_file(&path, NDFileOpenMode::WRITE, &comp)
            .unwrap();
        writer.write_file(&Arc::new(comp)).unwrap();
        writer.close_file().unwrap();

        let h5 = H5File::open(&path).unwrap();
        let ds = h5.dataset("entry/instrument/detector/data").unwrap();
        // HDF5 axes are the NDArray dims reversed (fastest-varying last), so a
        // [4, 5] frame is stored as [5, 4]; the flat pixel buffer order is
        // unchanged and round-trips through the reversed filter intact.
        assert_eq!(ds.shape(), vec![5, 4]);
        assert!(ds.is_chunked());
        let read = ds.read_raw::<u16>().unwrap();
        assert_eq!(
            read, expect,
            "LZ4 direct-chunk-write must reverse to the original pixels"
        );
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn test_direct_chunk_write_blosc_roundtrips() {
        let path = temp_path("hdf5_dcw_blosc");
        let mut writer = Hdf5Writer::new();
        let orig = ramp_u16(4, 5);
        let expect = u16_pixels(&orig);
        let comp =
            crate::codec::compress_blosc(&orig, &crate::codec::BloscConfig::default()).unwrap();
        assert_eq!(comp.codec.as_ref().unwrap().name, CodecName::Blosc);
        writer
            .open_file(&path, NDFileOpenMode::WRITE, &comp)
            .unwrap();
        writer.write_file(&Arc::new(comp)).unwrap();
        writer.close_file().unwrap();

        let h5 = H5File::open(&path).unwrap();
        let ds = h5.dataset("entry/instrument/detector/data").unwrap();
        assert_eq!(ds.shape(), vec![5, 4]);
        let read = ds.read_raw::<u16>().unwrap();
        assert_eq!(read, expect, "BLOSC direct-chunk-write must round-trip");
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn test_direct_chunk_write_bslz4_roundtrips() {
        // `compress_bslz4` emits the canonical bitshuffle+LZ4 on-disk format
        // (byte-for-byte the libhdf5/h5py/C-areaDetector bytes — locked by
        // codec::tests::test_bitshuffle_matches_c_reference_vector), stored
        // verbatim with the 12-byte chunk header (test_codec_chunk_bytes_bslz4_header).
        // rust-hdf5 0.2.21's bitshuffle reverse filter is canonical (LSB-first),
        // so it reads those bytes back to the original pixels. Use a whole-block
        // frame: u16 blocks are 4096 elems, so 128*128 = 16384 = 4 full blocks
        // with no partial tail.
        let path = temp_path("hdf5_dcw_bslz4");
        let mut writer = Hdf5Writer::new();
        let orig = ramp_u16(128, 128);
        let expect = u16_pixels(&orig);
        let comp = crate::codec::compress_bslz4(&orig).unwrap();
        assert_eq!(comp.codec.as_ref().unwrap().name, CodecName::BSLZ4);
        writer
            .open_file(&path, NDFileOpenMode::WRITE, &comp)
            .unwrap();
        writer.write_file(&Arc::new(comp)).unwrap();
        writer.close_file().unwrap();

        let h5 = H5File::open(&path).unwrap();
        let ds = h5.dataset("entry/instrument/detector/data").unwrap();
        assert_eq!(ds.shape(), vec![128, 128]);
        assert!(ds.is_chunked());
        assert_eq!(ds.element_size(), 2, "dataset keeps the original u16 type");
        let read = ds.read_raw::<u16>().unwrap();
        assert_eq!(read, expect, "BSLZ4 direct-chunk-write must round-trip");
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn test_codec_chunk_bytes_bslz4_header() {
        // The HDF5 bitshuffle filter expects a 12-byte big-endian chunk header ahead of the
        // canonical stream: uncompressed total bytes (u64) and the block size in bytes (u32 =
        // block_elems * elem_size, C hardcodes the 8192 default), per
        // `NDFileHDF5Dataset::writeFile` (`NDFileHDF5Dataset.cpp:316-328`).
        let codec = Codec {
            name: CodecName::BSLZ4,
            compressed_size: 0,
            level: 0,
            shuffle: 0,
            compressor: 0,
            original_data_type: NDDataType::UInt16,
        };
        let payload = [0xDEu8, 0xAD, 0xBE, 0xEF];
        let total = 128 * 128 * 2; // one u16 frame
        let out = codec_chunk_bytes(&codec, total, &payload);
        assert_eq!(&out[0..8], &(total as u64).to_be_bytes());
        // u16 default block is 4096 elems => 8192 bytes (matches C's 8192).
        assert_eq!(&out[8..12], &8192u32.to_be_bytes());
        assert_eq!(&out[12..], &payload, "canonical stream follows verbatim");
    }

    #[test]
    fn test_direct_chunk_write_lz4_multiframe_extends_leading_axis() {
        let path = temp_path("hdf5_dcw_lz4_multi");
        let mut writer = Hdf5Writer::new();
        writer.set_num_capture(0);
        let frames: Vec<NDArray> = (0..3u16)
            .map(|f| {
                let data: Vec<u16> = (0..20u16).map(|i| i + f * 100).collect();
                NDArray::with_data(
                    vec![NDDimension::new(4), NDDimension::new(5)],
                    NDDataBuffer::from_vec::<u16>(data),
                )
            })
            .collect();
        let comp: Vec<NDArray> = frames
            .iter()
            .map(|f| crate::codec::compress_lz4(f).unwrap())
            .collect();
        writer
            .open_file(
                &path,
                NDFileOpenMode::WRITE | NDFileOpenMode::MULTIPLE,
                &comp[0],
            )
            .unwrap();
        for c in &comp {
            writer.write_file(&Arc::new(c.clone())).unwrap();
        }
        writer.close_file().unwrap();

        let h5 = H5File::open(&path).unwrap();
        let ds = h5.dataset("entry/instrument/detector/data").unwrap();
        assert_eq!(ds.shape(), vec![3, 5, 4], "three compressed frames stacked");
        let read = ds.read_raw::<u16>().unwrap();
        let mut expect = Vec::new();
        for f in 0..3u16 {
            for i in 0..20u16 {
                expect.push(i + f * 100);
            }
        }
        assert_eq!(read, expect, "every frame's pixels recovered in order");
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn test_direct_chunk_write_jpeg_records_chunked_dataset() {
        // JPEG is lossy and the rust-hdf5 reader has no JPEG reverse filter, so
        // pixels cannot round-trip; verify the dataset is created with the right
        // original type/shape and chunked (one whole frame per chunk). The JPEG
        // filter id is covered by test_frame_codec_overrides_the_scheme.
        let path = temp_path("hdf5_dcw_jpeg");
        let mut writer = Hdf5Writer::new();
        let mono: Vec<u8> = (0..64u16).map(|i| (i * 3) as u8).collect();
        let src = NDArray::with_data(
            vec![NDDimension::new(8), NDDimension::new(8)],
            NDDataBuffer::from_vec::<u8>(mono),
        );
        let comp = crate::codec::compress_jpeg(&src, 90).expect("jpeg encode");
        assert_eq!(comp.codec.as_ref().unwrap().name, CodecName::JPEG);
        writer
            .open_file(&path, NDFileOpenMode::WRITE, &comp)
            .unwrap();
        writer.write_file(&Arc::new(comp)).unwrap();
        writer.close_file().unwrap();

        let h5 = H5File::open(&path).unwrap();
        let ds = h5.dataset("entry/instrument/detector/data").unwrap();
        assert_eq!(ds.shape(), vec![8, 8]);
        assert!(ds.is_chunked());
        std::fs::remove_file(&path).ok();
    }

    fn with_color_mode(mut frame: NDArray, mode: NDColorMode) -> NDArray {
        frame.attributes.add(NDAttribute::new_static(
            "ColorMode",
            "",
            NDAttrSource::Driver,
            NDAttrValue::Int32(mode as i32),
        ));
        frame
    }

    /// C's JPEG filter (NDFileHDF5.cpp:3439-3465): `[quality, ncols, nrows,
    /// rgb]` for a 2-D Mono or 3-D RGB1 frame, and no filter or codec for any
    /// other.
    #[test]
    fn jpeg_takes_its_geometry_from_a_mono_or_rgb1_frame() {
        let mut s = Hdf5Settings::default();
        s.set_compression_type(COMPRESS_JPEG);
        let dims = |d: &[usize]| d.iter().map(|&n| NDDimension::new(n)).collect::<Vec<_>>();
        let frame = |d: &[usize]| NDArray::new(dims(d), NDDataType::UInt8);
        let cds = |f: &NDArray| {
            s.build_pipeline(1, f)
                .map(|p| p.filters[0].cd_values.clone())
        };
        let codec = |f: &NDArray| s.dataset_codec(NDDataType::UInt8, f).map(|c| c.name);

        let mono = frame(&[4, 5]);
        assert_eq!(cds(&mono), Some(vec![90, 4, 5, 0]));
        assert_eq!(codec(&mono), Some(CodecName::JPEG));
        let rgb1 = with_color_mode(frame(&[3, 4, 5]), NDColorMode::RGB1);
        assert_eq!(cds(&rgb1), Some(vec![90, 4, 5, 1]));
        assert_eq!(codec(&rgb1), Some(CodecName::JPEG));
        for other in [
            frame(&[3, 4, 5]),
            with_color_mode(frame(&[4, 5]), NDColorMode::RGB1),
            with_color_mode(frame(&[4, 3, 5]), NDColorMode::RGB2),
            frame(&[20]),
        ] {
            assert_eq!(cds(&other), None, "{:?}", other.dims);
            assert_eq!(codec(&other), None, "{:?}", other.dims);
        }
    }

    /// A pre-compressed JPEG frame outside the gate: C prints the line, sets
    /// no codec, and `verifyChunking` refuses the frame (NDFileHDF5.cpp:3455,
    /// NDFileHDF5Dataset.cpp:343-348).
    #[test]
    fn a_jpeg_frame_outside_the_color_mode_gate_is_refused() {
        let path = temp_path("hdf5_jpeg_gate");
        let mut writer = Hdf5Writer::new();
        let src = NDArray::with_data(
            vec![NDDimension::new(8), NDDimension::new(8)],
            NDDataBuffer::from_vec::<u8>(vec![7; 64]),
        );
        let comp = crate::codec::compress_jpeg(&src, 90).expect("jpeg encode");
        let comp = with_color_mode(comp, NDColorMode::RGB1);
        writer
            .open_file(&path, NDFileOpenMode::WRITE, &comp)
            .unwrap();
        let err = writer.write_file(&Arc::new(comp)).unwrap_err();
        assert_eq!(
            writer.open_posts.lock().lines,
            ["JPEG compression only supports 2-D mono and 3-D RGB1 modes\n"]
        );
        assert_eq!(err.lines(), [MISMATCHED, ABORTING]);
        writer.close_file().ok();
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn test_codec_change_mid_stream_errors() {
        // C verifyChunking refuses a compressed frame whose codec differs from
        // the dataset's (NDFileHDF5Dataset.cpp:194-200, :343-348).
        let path = temp_path("hdf5_codec_change");
        let mut writer = Hdf5Writer::new();
        let f0 = crate::codec::compress_lz4(&ramp_u16(4, 5)).unwrap();
        let f1 = crate::codec::compress_bslz4(&ramp_u16(4, 5)).unwrap();
        writer
            .open_file(&path, NDFileOpenMode::WRITE | NDFileOpenMode::MULTIPLE, &f0)
            .unwrap();
        writer.write_file(&Arc::new(f0)).unwrap();
        let err = writer.write_file(&Arc::new(f1)).unwrap_err();
        assert_eq!(err.lines(), [MISMATCHED, ABORTING]);
        writer.close_file().ok();
        std::fs::remove_file(&path).ok();
    }

    /// C's `writeFile` direct-chunk-writes a pre-compressed frame whatever
    /// the file's SWMR mode (NDFileHDF5Dataset.cpp:291-331), on one frame
    /// axis or an extra-dimension grid.
    #[test]
    fn a_swmr_file_direct_chunk_writes_a_compressed_frame() {
        let frames = [
            crate::codec::compress_lz4(&ramp_u16(4, 5)).unwrap(),
            crate::codec::compress_bslz4(&ramp_u16(4, 5)).unwrap(),
            crate::codec::compress_zlib(&ramp_u16(4, 5), 4).unwrap(),
        ];
        for (comp, extra_dims) in frames.iter().flat_map(|c| [(c.clone(), 0), (c.clone(), 1)]) {
            let what = format!(
                "{:?} extra dims {extra_dims}",
                comp.codec.as_ref().unwrap().name
            );
            let path = temp_path("hdf5_swmr_direct_chunk");
            let mut writer = Hdf5Writer::new();
            writer.set_swmr_mode(true);
            writer.set_num_capture(0);
            writer.set_n_extra_dims(extra_dims);
            writer.set_extra_dim_size(0, 3);
            writer
                .open_file(
                    &path,
                    NDFileOpenMode::WRITE | NDFileOpenMode::MULTIPLE,
                    &comp,
                )
                .unwrap();
            for _ in 0..3 {
                writer.write_file(&Arc::new(comp.clone())).unwrap();
            }
            writer.close_file().unwrap();
            let h5 = H5File::open(&path).unwrap();
            let ds = h5.dataset("entry/instrument/detector/data").unwrap();
            let leading: &[usize] = if extra_dims == 0 { &[3] } else { &[1, 3] };
            assert_eq!(ds.shape(), [leading, &[5, 4]].concat(), "{what}");
            assert_eq!(
                ds.read_raw::<u16>().unwrap(),
                u16_pixels(&ramp_u16(4, 5)).repeat(3),
                "{what}"
            );
            std::fs::remove_file(&path).ok();
        }
    }

    /// `verifyChunking` holds a SWMR file too (NDFileHDF5Dataset.cpp:343-348).
    #[test]
    fn a_swmr_file_refuses_a_compressed_frame_that_is_not_one_chunk() {
        let path = temp_path("hdf5_swmr_comp_mismatch");
        let mut writer = Hdf5Writer::new();
        writer.set_swmr_mode(true);
        writer.set_chunk_size_auto(false);
        writer.set_n_row_chunks(2);
        let comp = crate::codec::compress_lz4(&ramp_u16(4, 5)).unwrap();
        writer
            .open_file(
                &path,
                NDFileOpenMode::WRITE | NDFileOpenMode::MULTIPLE,
                &comp,
            )
            .unwrap();
        let err = writer.write_file(&Arc::new(comp)).unwrap_err();
        assert_eq!(err.lines(), [MISMATCHED, ABORTING]);
        writer.close_file().ok();
        std::fs::remove_file(&path).ok();
    }

    /// `verifyChunking` refuses on the extraDimChunk params as written, so on
    /// N's, which nFramesChunks replaces, and on one an axis of size 1 clamps
    /// to 1 (C oracle: ExtraDimChunkX 2 on X=1 refuses the first LZ4 frame).
    #[test]
    fn a_compressed_frame_is_refused_on_any_extra_dim_chunk_over_one() {
        for swmr in [false, true] {
            for (extra_dims, axis) in [(0, 0), (1, 1)] {
                let path = temp_path(&format!("hdf5_comp_ed_chunk_{swmr}_{axis}"));
                let mut writer = Hdf5Writer::new();
                writer.set_swmr_mode(swmr);
                writer.set_n_extra_dims(extra_dims);
                writer.set_extra_dim_size(0, 2);
                writer.set_extra_dim_chunk(axis, 2);
                writer.set_num_capture(2);
                let comp = crate::codec::compress_lz4(&ramp_u16(4, 5)).unwrap();
                writer
                    .open_file(
                        &path,
                        NDFileOpenMode::WRITE | NDFileOpenMode::MULTIPLE,
                        &comp,
                    )
                    .unwrap();
                let err = writer.write_file(&Arc::new(comp)).unwrap_err();
                assert_eq!(
                    err.lines(),
                    [MISMATCHED, ABORTING],
                    "swmr={swmr} axis={axis}"
                );
                writer.close_file().ok();
                std::fs::remove_file(&path).ok();
            }
        }
    }

    /// C's line for a compressed frame `verifyChunking` refuses
    /// (NDFileHDF5Dataset.cpp:343-348).
    const MISMATCHED: &str = "NDFileHDF5Dataset::writeFile ERROR Unable to write \
                              pre-compressed data - mismatched chunk definition\n";

    /// C `writeFile`'s line when the dataset refuses a frame
    /// (NDFileHDF5.cpp:1521-1523).
    const ABORTING: &str = "NDFileHDF5::writeFile ERROR: could not write to dataset. Aborting\n";

    #[test]
    fn a_refused_frame_aborts_and_closes_with_the_earlier_frames_on_disk() {
        // C writes each frame as it comes, so the file the abort closes
        // (NDFileHDF5.cpp:1519-1557) holds every frame before the refused one.
        let path = temp_path("hdf5_abort");
        let mut writer = Hdf5Writer::new();
        writer.set_num_capture(0);
        writer.set_chunk_size_auto(false);
        writer.set_n_row_chunks(8);
        writer.set_n_col_chunks(8);
        writer.set_n_frames_chunks(4);
        let plain = ramp_u16(8, 8);
        writer
            .open_file(
                &path,
                NDFileOpenMode::WRITE | NDFileOpenMode::MULTIPLE,
                &plain,
            )
            .unwrap();
        writer.write_file(&Arc::new(plain.clone())).unwrap();
        writer.write_file(&Arc::new(plain.clone())).unwrap();
        let comp = crate::codec::compress_lz4(&plain).unwrap();
        let err = writer.write_file(&Arc::new(comp)).unwrap_err();
        assert!(err.is_aborted());
        assert_eq!(err.lines(), [MISMATCHED, ABORTING]);
        assert!(writer.handle.is_none() && writer.current_path.is_none());

        let h5 = H5File::open(&path).unwrap();
        let ds = h5.dataset("entry/instrument/detector/data").unwrap();
        assert_eq!(ds.shape(), vec![2, 8, 8]);
        let one = u16_pixels(&plain);
        assert_eq!(ds.read_raw::<u16>().unwrap(), [one.clone(), one].concat());
        std::fs::remove_file(&path).ok();
    }

    /// C keeps the user's chunk params for a pre-compressed first frame
    /// (NDFileHDF5.cpp:3234-3271), so `verifyChunking` refuses a frame that is
    /// not exactly one chunk (NDFileHDF5Dataset.cpp:204-222, :343-348); a
    /// Single file has no frame-axis chunk to refuse on.
    #[test]
    fn a_compressed_first_frame_keeps_the_user_chunking() {
        let path = temp_path("hdf5_comp_chunking");
        let comp = crate::codec::compress_lz4(&ramp_u16(8, 8)).unwrap();
        let write = |mode, set: &dyn Fn(&mut Hdf5Writer)| {
            let mut writer = Hdf5Writer::new();
            writer.set_num_capture(0);
            set(&mut writer);
            writer.open_file(&path, opened(mode), &comp).unwrap();
            let r = writer.write_file(&Arc::new(comp.clone()));
            writer.close_file().ok();
            std::fs::remove_file(&path).ok();
            r.map_err(|e| e.lines().to_vec())
        };
        let refused = Err(vec![MISMATCHED.to_owned(), ABORTING.to_owned()]);
        assert_eq!(
            write(NDFileMode::Stream, &|w| w.set_n_frames_chunks(2)),
            refused
        );
        assert_eq!(
            write(NDFileMode::Stream, &|w| {
                w.set_chunk_size_auto(false);
                w.set_n_row_chunks(4);
            }),
            refused
        );
        assert_eq!(
            write(NDFileMode::Single, &|w| w.set_n_frames_chunks(4)),
            Ok(())
        );
        assert_eq!(write(NDFileMode::Stream, &|_| {}), Ok(()));
    }

    fn renamed(mut a: NDArray, name: &str) -> NDArray {
        a.codec.as_mut().unwrap().name = CodecName::from_name(name);
        a
    }

    #[test]
    fn zlib_and_lz4hdf5_frames_override_the_scheme() {
        // C configureCompression switches the scheme to the frame's codec
        // (NDFileHDF5.cpp:3326-3334), so the frame matches and is written.
        let frames = [
            crate::codec::compress_zlib(&ramp_u16(4, 5), 4).unwrap(),
            crate::codec::compress_lz4hdf5(&ramp_u16(4, 5), 0).unwrap(),
        ];
        for comp in frames {
            let name = comp.codec.as_ref().unwrap().name.as_str().to_owned();
            let path = temp_path(&format!("hdf5_override_{name}"));
            let mut writer = Hdf5Writer::new();
            writer
                .open_file(&path, NDFileOpenMode::WRITE, &comp)
                .unwrap();
            writer.write_file(&Arc::new(comp)).unwrap();
            writer.close_file().unwrap();
            let h5 = H5File::open(&path).unwrap();
            let ds = h5.dataset("entry/instrument/detector/data").unwrap();
            let read = ds.read_raw::<u16>().unwrap();
            assert_eq!(read, u16_pixels(&ramp_u16(4, 5)), "{name}");
            std::fs::remove_file(&path).ok();
        }
    }

    #[test]
    fn szip_and_nbit_frames_direct_chunk_write_when_the_codec_matches() {
        // C's szip and nbit schemes leave the dataset codec at the bare name
        // (NDFileHDF5.cpp:3365, :3380); an equal frame is written verbatim.
        for (scheme, name) in [(2, "szip"), (1, "nbit")] {
            let path = temp_path(&format!("hdf5_dcw_{name}"));
            let mut writer = Hdf5Writer::new();
            writer.set_compression_type(scheme);
            let frame = renamed(crate::codec::compress_lz4(&ramp_u16(4, 5)).unwrap(), name);
            let mut off = frame.clone();
            off.codec.as_mut().unwrap().level = 1;
            writer
                .open_file(
                    &path,
                    NDFileOpenMode::WRITE | NDFileOpenMode::MULTIPLE,
                    &frame,
                )
                .unwrap();
            writer.write_file(&Arc::new(frame)).unwrap();
            let err = writer.write_file(&Arc::new(off)).unwrap_err();
            assert_eq!(err.lines(), [MISMATCHED, ABORTING], "{name}");
            writer.close_file().unwrap();
            std::fs::remove_file(&path).ok();
        }
    }

    #[test]
    fn an_unknown_codec_opens_and_the_frame_gets_the_mismatched_line() {
        let path = temp_path("hdf5_dcw_unknown");
        let mut writer = Hdf5Writer::new();
        let frame = renamed(crate::codec::compress_lz4(&ramp_u16(4, 5)).unwrap(), "foo");
        writer
            .open_file(
                &path,
                NDFileOpenMode::WRITE | NDFileOpenMode::MULTIPLE,
                &frame,
            )
            .unwrap();
        let err = writer.write_file(&Arc::new(frame)).unwrap_err();
        assert_eq!(err.lines(), [MISMATCHED, ABORTING]);
        writer.close_file().unwrap();
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn an_uncompressed_frame_goes_through_the_dataset_filter() {
        // verifyChunking fails on the codec and an uncompressed frame takes
        // H5Dwrite through the dataset's filter (NDFileHDF5Dataset.cpp:350).
        let path = temp_path("hdf5_codec_then_plain");
        let mut writer = Hdf5Writer::new();
        writer.set_num_capture(0);
        let f0 = crate::codec::compress_lz4(&ramp_u16(4, 5)).unwrap();
        writer
            .open_file(&path, NDFileOpenMode::WRITE | NDFileOpenMode::MULTIPLE, &f0)
            .unwrap();
        writer.write_file(&Arc::new(f0)).unwrap();
        writer.write_file(&Arc::new(ramp_u16(4, 5))).unwrap();
        writer.close_file().unwrap();
        let h5 = H5File::open(&path).unwrap();
        let ds = h5.dataset("entry/instrument/detector/data").unwrap();
        assert_eq!(ds.shape(), vec![2, 5, 4]);
        let one = u16_pixels(&ramp_u16(4, 5));
        assert_eq!(ds.read_raw::<u16>().unwrap(), [one.clone(), one].concat());
        std::fs::remove_file(&path).ok();
    }

    /// C's `openFile` lines for a missing layout file and a layout that does
    /// not parse, both from `verifyLayoutXMLFile` ahead of the file
    /// (NDFileHDF5.cpp:2155-2172, :213-217), and a file it cannot create
    /// (:3846-3851, :271-275).
    #[test]
    fn a_failed_open_carries_c_open_file_lines() {
        let arr = NDArray::new(vec![NDDimension::new(4)], NDDataType::UInt8);

        let mut writer = Hdf5Writer::new();
        writer.set_layout_filename("/nonexistent/layout.xml");
        let path = temp_path("hdf5_no_layout");
        let err = writer
            .open_file(&path, NDFileOpenMode::WRITE, &arr)
            .unwrap_err();
        assert_eq!(err.lines(), [LayoutFault::CannotOpen.line()]);
        assert!(!path.exists());

        let mut writer = Hdf5Writer::new();
        writer.set_layout_filename("<?xml version=\"1.0\"?><hdf5_layout>");
        let path = temp_path("hdf5_bad_layout");
        let err = writer
            .open_file(&path, NDFileOpenMode::WRITE, &arr)
            .unwrap_err();
        assert_eq!(err.lines(), [LayoutFault::ParserError.line()]);
        assert!(!path.exists());

        let path = Path::new("/nonexistent/dir/f.h5");
        for mode in [NDFileMode::Single, NDFileMode::Stream] {
            let mut writer = Hdf5Writer::new();
            writer.set_swmr_mode(mode == NDFileMode::Stream);
            let err = writer.open_file(path, opened(mode), &arr).unwrap_err();
            assert_eq!(
                err.lines(),
                [
                    "NDFileHDF5::createNewFile Unable to create HDF5 file: /nonexistent/dir/f.h5\n",
                    "NDFileHDF5::openFile ERROR Failed to create a new output file\n",
                ]
            );
        }
    }
}
