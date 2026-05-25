# ad-plugins-rs file-writer plugins — review vs ADCore C++

Scope: `file_hdf5.rs`, `file_jpeg.rs`, `file_tiff.rs`, `file_netcdf.rs`,
`file_nexus.rs`, `file_magick.rs`, plus `plugin/file_base.rs` and
`plugin/file_controller.rs` in `ad-core-rs` (the shared NDPluginFile
control plane), and the IOC wiring in `ioc.rs`.

C++ reference: `epics-modules/ADCore/ADApp/pluginSrc/NDFile*.cpp/.h`,
`NDPluginFile.cpp/.h`.

All Cargo features needed by the file plugins are unconditionally
enabled (`rust-hdf5` with `all_filters`, `tiff`, `jpeg-encoder`,
`jpeg-decoder`, `netcdf3`, `image`). None of these plugins are behind
optional feature flags, so none are "intentionally stubbed at the
crate level" — every gap below is a real functional gap, not a
feature-flag artifact. (`NDFileNexus` is grouped under the
"Stub plugins" comment in `ioc.rs:460-493`, but it is wired to a
real `NexusFileProcessor`, not the passthrough — see HDF5/NeXus notes.)

---

## Feature Gaps

### HDF5 (`file_hdf5.rs`)

- **HDF5 layout XML completely unimplemented.** C++ `createXMLFileLayout()`
  / `NDFileHDF5LayoutXML.cpp` build the entire group/dataset tree from a
  user XML file (`HDF5_layoutFilename`). The Rust port registers the
  params `HDF5_layoutFilename`, `HDF5_layoutValid`, `HDF5_layoutErrorMsg`
  (`file_hdf5.rs:582-584`) but never reads them — `on_param_change` has no
  branch for them and there is no XML parser. Output is always a flat
  dataset named `data` at the file root. Consequence: any site using a
  custom HDF5 layout (most production beamlines) gets a structurally
  different file.

- **NDAttribute datasets not implemented.** C++ `createAttributeDataset()`
  / `writeAttributeDataset()` (`NDFileHDF5.cpp:2707`, `2927`) and
  `NDFileHDF5AttributeDataset.cpp` create one HDF5 *dataset* per
  NDAttribute, extended once per frame (the `ndattribute` datasets).
  The Rust port instead writes each attribute as an HDF5 *attribute*
  (scalar string) on the image dataset (`file_hdf5.rs:322-335`), only
  capturing the value at dataset-creation time. Consequences: (a) per-frame
  attribute time-series are lost — only one value is recorded; (b) all
  attributes are stringified regardless of NDAttrValue type, losing numeric
  typing; (c) `HDF5_NDAttributeChunk` / `HDF5_dimAttDatasets` params are
  registered but unused.

- **Performance dataset not implemented.** C++ `writePerformanceDataset()`
  (`NDFileHDF5.cpp:2677`) writes a `timestamp` dataset with per-frame I/O
  timing. The Rust port accumulates `total_runtime` / `total_bytes` and
  pushes them to `HDF5_totalRuntime` / `HDF5_totalIoSpeed` params
  (`file_hdf5.rs:654-671`), but writes **no** performance dataset into the
  file. `HDF5_storePerformance` only gates the param math.

- **Extra dimensions (`HDF5_nExtraDims`, `extraDimSizeN..9`, position
  placement) unimplemented.** ~40 `HDF5_extraDim*` / `HDF5_posName*` /
  `HDF5_posIndex*` params are registered (`file_hdf5.rs:546-619`) but never
  consulted. C++ supports an N-dimensional hyperslab layout driven by these.
  Rust always writes a single leading frame index (SWMR) or a flat
  per-frame dataset (standard).

- **Chunking params unused.** `HDF5_chunkSizeAuto`, `HDF5_nRowChunks`,
  `HDF5_nColChunks`, `HDF5_nFramesChunks`, `HDF5_chunkSize2..9`,
  `HDF5_chunkBoundaryAlign/Threshold` are all registered and ignored.
  Standard-mode chunk size is hard-coded to the full frame shape
  (`file_hdf5.rs:298` `.chunk(&shape[..])`); SWMR chunking is whatever
  `SwmrFileWriter` defaults to. C++ honours user chunk geometry, which
  matters for read performance and compression ratio.

- **`HDF5_fillValue` unused.** Registered (`file_hdf5.rs:610`), never applied
  to dataset creation property list.

- **BLOSC sub-compressor coverage:** Rust maps `bslz4` only implicitly.
  `COMPRESS_BSHUF` (value 5, `file_hdf5.rs:25`) is declared `#[allow(dead_code)]`
  and has **no** arm in `build_pipeline` — selecting bitshuffle compression
  yields `None` (no compression) silently. C++ supports it.

- **Compression only applied in standard mode.** `build_pipeline` is called
  only by `write_standard` (`file_hdf5.rs:290`). The SWMR path
  (`open_swmr` / `write_swmr`) creates the streaming dataset with no filter
  pipeline — compression is silently dropped whenever `HDF5_SWMRMode` is on.

### NeXus (`file_nexus.rs`)

- **XML template ignored.** C++ `NDFileNexus` is driven entirely by a
  user XML template (`loadTemplateFile()`, `NDFileNexus.cpp:815`); the
  template defines the whole NXentry/NXdata tree, attribute mapping, and
  per-node datatypes. The Rust port registers `NEXUS_TEMPLATE_PATH`,
  `NEXUS_TEMPLATE_FILE`, `NEXUS_TEMPLATE_VALID` (and dup'd
  `TEMPLATE_FILE_*`) at `file_nexus.rs:330-335` but never reads them and
  has no XML parser. It always emits a fixed hard-coded hierarchy. This
  means the central feature of `NDFileNexus` is absent.

- **NX_class stored as a child dataset, not a group attribute.** Real NeXus
  requires `NX_class` to be an HDF5 *group attribute*. The Rust port
  creates a child `u8` dataset called `NX_class` with a string attribute
  `value` (`file_nexus.rs:56-75`), because `rust-hdf5` cannot write group
  attributes. NeXus-aware readers (nexpy, h5py NeXus, DAWN) will not
  recognise these groups. The code comments admit this.

- **Per-frame metadata mangled.** `uniqueId`/`timeStamp` are written as one
  HDF5 attribute *per frame* with a numbered name (`uniqueId_0`,
  `uniqueId_1`, …, `file_nexus.rs:224-237`). C++ writes them as proper
  datasets. An N-frame file accumulates 2N attributes on one dataset.

### NetCDF (`file_netcdf.rs`)

- **Attributes are not per-frame and lose typing/metadata.** C++
  `NDFileNetCDF.cpp:210-330` writes, for each NDAttribute, a *record
  variable* `Attr_<name>` (one value per frame along the `numArrays`
  dimension) plus four global text attributes `Attr_<name>_DataType`,
  `_Description`, `_Source`, `_SourceType`. The Rust port writes each
  attribute as a single static string variable-attribute on `array_data`
  (`file_netcdf.rs:250-257`), keeping only the first frame's value, no
  type, no description, no source. Consequence: time-resolved attribute
  data is lost and the file does not match the documented NetCDF schema.

- **`epicsTSSec` / `epicsTSNsec` not written.** C++ writes four per-frame
  metadata record variables: `uniqueId`, `timeStamp`, `epicsTSSec`,
  `epicsTSNsec` (`NDFileNetCDF.cpp:183-198`). Rust writes only `uniqueId`
  and `timeStamp` (`file_netcdf.rs:261-264`), and only for multi-frame
  files.

- **`numArrays` dimension omitted for single-frame files.** C++ always
  defines `array_data` with `pArray->ndims+1` dimensions, dim0 = `numArrays`
  (size 1 for single, `NC_UNLIMITED` for multiple) — `NDFileNetCDF.cpp:118,202`.
  Rust omits the leading dimension entirely when `frames.len() == 1`
  (`file_netcdf.rs:233-242`). A single-frame Rust file and a single-frame
  C++ file have different rank for `array_data`; a reader expecting the C++
  schema will misread it.

### JPEG (`file_jpeg.rs`)

- No `NDFileJPEG`-specific gaps beyond format limits; C++ JPEG also only
  supports 8-bit and `supportsMultipleArrays = 0` (`NDFileJPEG.h:326`),
  which the Rust port matches (`file_jpeg.rs:160`). See Bugs for the
  quality-default mismatch.

### TIFF (`file_tiff.rs`)

- **Standard TIFF tags not written.** C++ writes `TIFFTAG_SOFTWARE`
  ("EPICS areaDetector"), `TIFFTAG_MODEL` (from a `Model` attribute),
  `TIFFTAG_MAKE` (from `Manufacturer`), `TIFFTAG_IMAGEDESCRIPTION` (from a
  `TIFFImageDescription` attribute), plus `EPICSTSSec`/`EPICSTSNsec` custom
  tags 65002/65003 (`NDFileTIFF.cpp:227-266`). Rust writes only `uniqueId`
  (65000) and `timestamp` (65001) and the attribute tags — no EPICS
  timestamp tags, no Software/Model/Make/ImageDescription. See Bugs for the
  tag-number mismatch, which is more serious.

- **No BigTIFF support.** The `tiff` crate `TiffEncoder::new` writes classic
  (32-bit-offset) TIFF only. C++ libtiff produces classic TIFF too by
  default, so this is parity for typical sizes but >4 GB files will fail in
  both; no explicit gap, noted for completeness.

### Magick (`file_magick.rs`)

- **`MAGICK_COMPRESS_TYPE` is a no-op.** Registered (`file_magick.rs:329`)
  and explicitly discarded in `on_param_change` (`file_magick.rs:352-356`).
  C++ NDFileMagick passes the compression type to GraphicsMagick.

- **`MAGICK_BIT_DEPTH` is effectively a no-op.** Stored on the writer
  (`set_bit_depth`) but `array_to_image` never reads `self.bit_depth`;
  output depth is dictated solely by the NDArray data type. C++ uses the
  bit-depth param to down/up-sample.

- **No multi-frame support** — `supports_multiple_arrays()` returns false
  (`file_magick.rs:288`). C++ NDFileMagick also has `supportsMultipleArrays=0`,
  so this is parity.

### Shared control plane (`file_base.rs` / `file_controller.rs`)

- **`FilePathExists` only updated on FilePath change.** C++ NDPluginFile
  re-checks file path existence and also exposes `checkPath()`. The Rust
  controller updates `file_path_exists` only inside the `file_path`
  param branch (`file_controller.rs:209-218`); it is never refreshed
  before a write, so a path deleted after being set still reads "exists".

- **No file-exists / overwrite check before Single write.** C++
  `NDPluginFile::writeFileBase` warns/refuses to overwrite depending on
  configuration; the Rust `process_array` Single path
  (`file_base.rs:206-226`) unconditionally creates and overwrites.

- **`NUM_CAPTURE` capture-buffer is not pre-sized / not memory-bounded.**
  C++ `NDPluginFile` pre-allocates the capture buffer and reports failure
  if the pool cannot satisfy it. Rust `capture_buffer` is a plain `Vec`
  that grows on each frame (`file_base.rs:228-232`); `NumCapture` only
  triggers the flush threshold. No NDArray-pool reservation, no failure
  signalling if memory is short.

- **`DeleteDriverFile` not honoured in Capture mode.** The Single and
  Stream branches delete the driver file (`file_base.rs:215-222`,
  `248-255`); `flush_capture` does not. C++ deletes per frame in all modes.

---

## Bugs

### CRITICAL

**HDF5 standard (non-SWMR) multi-frame mode writes one dataset per frame
instead of one extensible dataset** — `file_hdf5.rs:282-286`.

```rust
let dataset_name = if self.frame_count == 0 {
    self.dataset_name.clone()
} else {
    format!("{}_{}", self.dataset_name, self.frame_count)
};
```

C++ behaviour: `NDFileHDF5Dataset::writeFile` writes every frame into a
single dataset whose leading dimension is extended via `extendDataSet` /
`nextRecord_` (`NDFileHDF5Dataset.cpp:109-373`). The resulting file has one
3-D dataset `[nframes, Y, X]`.

Rust behaviour (whenever `swmr_mode` is off, which is the default —
`Hdf5Writer::new` sets `swmr_mode: false`): the first frame goes to `data`,
the second to `data_1`, the third to `data_2`, etc. Each is a separate 2-D
dataset.

Consequence: every non-SWMR HDF5 file written in Stream or Capture mode is
structurally incompatible with areaDetector/h5py expectations — readers
looking for a single `[nframes,…]` dataset see only the first frame. This
is the default path, so it is the most damaging bug in the set.

**TIFF attribute custom tags collide with the reserved standard-tag range
and use the wrong value format** — `file_tiff.rs:116-124`.

C++ reserves tags **65000–65003** for `NDTimeStamp`, `NDUniqueId`,
`EPICSTSSec`, `EPICSTSNsec`, and starts NDArray attribute tags at
**65010** (`NDFileTIFF.cpp:38-43`, `TIFFTAG_FIRST_ATTRIBUTE = 65010`). The
attribute value format is `name:value` (colon) and the field is registered
under the name `Attribute_<n>` (`NDFileTIFF.cpp:90`, `303-327`).

Rust uses tag 65000 for `uniqueId` and 65001 for `timestamp`
(`file_tiff.rs:137-152`) — colliding with C++'s `NDTimeStamp`(65000) and
`NDUniqueId`(65001) but with swapped meaning and `name=value`/`key=value`
text format. Attribute tags start at 65010 (`file_tiff.rs:122`) which is
correct, but the value separator is `=` not `:` (`file_tiff.rs:123`).

Consequence: a TIFF written by Rust and read by the C++ NDFileTIFF reader
(or vice-versa) mis-identifies the timestamp/uniqueId tags and fails to
parse attribute values. Round-tripping only works Rust↔Rust.

### HIGH

**NetCDF `array_data` rank differs from C++ for single-frame files** —
`file_netcdf.rs:233-245`. C++ always gives `array_data` the leading
`numArrays` dimension (rank = `ndims+1`). Rust omits it for single-frame
files. Files are not schema-compatible. (Listed under Feature Gaps too;
it is a concrete wire-format bug.)

**NetCDF unsigned 16/32-bit data is silently truncated through signed
storage with no recovery on the write side guarantee** —
`file_netcdf.rs:80-88`, `121-135`. `U16` is cast element-wise to `i16`
(`x as i16`) and stored as `NC_SHORT`; `U32` → `i32`. C++ does the same
*bit-reinterpretation* via `NC_SHORT`/`NC_INT` but the values are stored
as raw bit patterns and the `dataType` global attribute lets readers
recover the unsigned type. The Rust cast `x as i16` is a *value-preserving
wrap* which for a `u16` like `0xFFFF` produces `-1` — same bit pattern, so
on read `(-1) as u16 == 0xFFFF` recovers correctly **only because**
`read_file` re-casts (`file_netcdf.rs:382-389`). This works Rust↔Rust but:
the `read_file` recovery is keyed on the `dataType` global attribute,
and that attribute stores `first.data_type as i32` — the Rust
`NDDataType` enum ordinal, **not** the C `NDDataType_t` value. If the two
enums diverge in ordering, recovery picks the wrong type. Verify
`NDDataType` ordinals match C `NDDataType_t` (NDInt8=0…NDFloat64=…); if not,
this is a silent type-corruption bug on read.

**NetCDF Int64/UInt64 written as `f64` lose precision for large magnitudes**
— `file_netcdf.rs:91-98`, `142-153`. This matches C++ (`NDFileNetCDF.cpp:171-172`
also casts to `NC_DOUBLE`), so it is parity, not a regression — noted
because values above 2^53 are corrupted in both. Not a Rust-only bug.

**HDF5 SWMR mode silently drops compression** — `open_swmr`
(`file_hdf5.rs:237-273`) never calls `build_pipeline`. With
`HDF5_SWMRMode=1` and any `HDF5_compressionType`, the user gets an
uncompressed file with no error. C++ applies the filter pipeline to SWMR
datasets.

**JPEG default quality inconsistent and not initialised as a param value**
— C++ `NDFileJPEG` sets `NDFileJPEGQuality` default to **50**
(`NDFileJPEG.cpp:327`). The Rust IOC wiring constructs
`JpegFileProcessor::new(90)` (`ioc.rs:338`) → writer default 90, while
`JpegFileProcessor::default()` uses 50 (`file_jpeg.rs:183`). Also
`register_params` creates `JPEG_QUALITY` (`file_jpeg.rs:201`) but never
calls `set_int32_param` to seed it with a default, so the readback PV
shows 0 until the user writes it, while the actual encoder uses 90.
Result: PV value (0) and effective quality (90) disagree, and the
effective default (90) differs from C++ (50).

### MEDIUM

**HDF5 standard-mode chunked write assumes one chunk per frame and writes
chunk index 0 every time** — `file_hdf5.rs:313-316`:
`ds.write_chunk(0, array.data.as_u8_slice())`. Because each frame gets its
own dataset (the CRITICAL bug above), index 0 is "correct" only as a
consequence of that bug. Once datasets are unified, this must become a
proper offset write. Also `write_chunk` is fed `as_u8_slice()` — the raw
byte buffer — bypassing the typed `write_raw` path; this is only safe if
the chunk is exactly the whole dataset and the host endianness matches the
file. For a multi-chunk or big-endian-file scenario it corrupts data.

**HDF5 `read_file` cannot read most data types** — `file_hdf5.rs:442-460`
only attempts `u8`, `u16`, `f64`. Reading an `i16`/`i32`/`u32`/`f32`/`i8`
HDF5 file falls through to `"unsupported HDF5 data type"`. C++ reads all
types. `ReadFile` on a Rust-written `i16` file fails.

**HDF5 `read_file` always reports type as UInt8/UInt16/Float64** —
even when it does succeed, an `i16` dataset read via the `u16` arm
(if HDF5 allows the raw read) is mis-typed. The reader infers type from
which `read_raw` succeeds, not from the dataset's actual HDF5 type.

**TIFF: signed RGB rejected** — `file_tiff.rs:182-185` etc. return an error
for `I8`/`I16`/`I32`/`I64` RGB. C++ libtiff handles signed RGB via
`SAMPLEFORMAT_INT`. Minor — signed RGB detectors are rare — but it is a
behavioural divergence (hard error vs success).

**TIFF colour-mode detection guesses from dims when no `ColorMode`
attribute** — `file_tiff.rs:34-41`. The fallback treats any `[3,_,_]`
array as RGB1. C++ also inspects `ColorMode` and falls back to dims, so
this is roughly parity, but the Rust fallback also fires for a genuine
3×N×M *mono* stack. Low risk; flagged for awareness.

**netCDF reader returns wrong dimension rank for record variables** —
`file_netcdf.rs:344-351` skips the unlimited dim when building `dims`, so a
multi-frame file read back yields a 2-D array (the frame shape) — correct
for "first frame" semantics, but the C++ reader returns the array with its
original `ndims` and relies on `numArrayDims`. Because Rust also omits
`numArrays` on single-frame writes, a single-frame file's `array_data` has
no unlimited dim and all dims are kept — consistent only by accident.

**`create_file_name` printf emulation is incomplete** — `file_base.rs:81-154`.
It handles `%s`, `%s`, `%d` with width/precision, but: a `%d` with an
explicit `-` (left-justify) flag is parsed into `spec` then ignored (always
zero-pads right-justified, `file_base.rs:131-136`); `%%` is not handled
(a literal `%%` becomes `%` + whatever follows mis-parsed). C++ uses real
`epicsSnprintf`. Most templates are simple so impact is low, but
left-justified or escaped templates misformat.

### LOW

**HDF5 `total_io_speed` uses MB = 1e6 bytes** — `file_hdf5.rs:662-666`
divides by `1_000_000`. C++ `writePerformanceDataset` / IO-speed math uses
the same decimal megabyte, so this is parity. Noted only to confirm it was
checked.

**NeXus `read_file` only handles u8/u16/f64** — `file_nexus.rs:260-274`,
same limitation as HDF5 `read_file`. i16/i32/u32/f32 NeXus files cannot be
re-read.

**Magick `F32` write clamps to `[0,1]` then scales to u16** —
`file_magick.rs:149-152`. Detector float data is not normalised to `[0,1]`;
any value >1.0 saturates to white and negatives to black. C++ NDFileMagick
scales by the actual data range. Silent data loss for float input.

**Magick I8/I16 reinterpreted as unsigned via `as u8`/`as u16`** —
`file_magick.rs:91`, `130`. A pixel of `-1_i8` becomes `255`. May be intended
(bit reinterpretation) but is undocumented and asymmetric with the
read path.

---

## Notes

- **Test coverage is writer-internal only.** Every plugin's tests exercise
  `Hdf5Writer`/`TiffWriter`/… directly and assert Rust-round-trip
  correctness. None verify the file against the documented C ADCore
  on-disk schema, which is why the CRITICAL HDF5-per-frame-dataset bug and
  the TIFF tag collision pass CI. The HDF5 `test_write_multiple_frames`
  test (`file_hdf5.rs:851-874`) only checks the file's magic bytes, not the
  dataset layout — it would still pass with the per-frame-dataset bug.

- **SWMR is the only HDF5 path that produces a correct multi-frame
  dataset**, but SWMR defaults off and the processor reports
  `HDF5_SWMRSupported = 1` unconditionally (`file_hdf5.rs:707-709`). A user
  who leaves SWMR off (the default) silently gets the broken layout.

- **`NDFileNexus` is listed under the "Stub plugins" comment block in
  `ioc.rs:460-493`** yet is wired to a real `NexusFileProcessor`. The
  processor is not a passthrough stub, but it ignores the XML template that
  defines NeXus output — so functionally it is a partial implementation
  mis-located in the stub section. The comment is misleading either way.

- **`netcdf3` crate limits NetCDF to the classic (v1/v2) format.** C++
  `NDFileNetCDF` also writes classic netCDF-3, so this is parity. 64-bit
  offset (large-file) netCDF is not produced by either, and netCDF-4/HDF5
  backing is out of scope for both.

- **Endianness:** HDF5 `write_chunk` / SWMR `append_frame` are fed
  `as_u8_slice()` (host-endian raw bytes). On a little-endian host writing
  a file later read on the same architecture this is fine; the typed
  `write_raw` path used for the uncompressed case is endian-correct. Mixed
  use within one file format is a latent portability hazard but not a bug
  on x86/ARM-LE.

- **Recommended fix priority:** (1) HDF5 single-extensible-dataset for
  standard mode — it is the default and breaks every multi-frame file;
  (2) TIFF tag numbers/format to match `TIFFTAG_*` constants;
  (3) NetCDF per-frame `Attr_<name>` record variables + `numArrays` leading
  dim on single-frame; (4) HDF5 SWMR compression; (5) JPEG default-quality
  param seeding. The XML-layout / NeXus-template engines are large features
  and can follow.
