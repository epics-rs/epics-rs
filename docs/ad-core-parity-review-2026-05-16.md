# ad-core-rs / ad-plugins-rs — ADCore Parity Review

Date: 2026-05-16. Reference: `~/codes/epics-modules/ADCore` (areaDetector ADCore).
Method: 4 parallel review agents, each crate area cross-checked file-by-file against
the C++ source. Detail reports:

- `crates/ad-core-rs/doc/parity-review-core.md` — NDArray / NDArrayPool / asynNDArrayDriver / ADDriver
- `crates/ad-core-rs/doc/parity-review-plugin-framework.md` — NDPluginDriver / NDPluginFile base
- `crates/ad-plugins-rs/doc/parity-review-compute.md` — 21 compute plugins
- `crates/ad-plugins-rs/doc/parity-review-files.md` — 6 file-writer plugins

---

## CRITICAL — wrong on-disk output / data corruption / acquisition stall

| ID | File | Defect |
|----|------|--------|
| F1 | `file_hdf5.rs:282-286` | Non-SWMR (default) HDF5 writes **one dataset per frame** (`data`, `data_1`, …). C++ extends a single dataset along a frame dimension. Every default-mode HDF5 file is unreadable by areaDetector/h5py — only frame 0 is seen. |
| C1 | `circular_buff.rs:149,208` | `post_remaining -= 1` on `usize` 0 when `post_count == 0`. Debug panic / release wrap to `usize::MAX` — capture sequence never completes. |
| PF1 | `plugin/.../NDArraySender::publish` | Reliable `tx.send().await` instead of C++ `trySend`+drop. A slow plugin (stalled HDF5 writer) backpressures up to the detector acquisition loop and stalls live acquisition. C++ drops frames instead. |
| F2 | `file_tiff.rs:137-152` | Custom TIFF tags 65000/65001 collide with C++ reserved 65000-65003 (`NDTimeStamp`/`NDUniqueId`/`EPICSTSSec`/`EPICSTSNsec`); meanings swapped, separator `=` vs C++ `:`. TIFF metadata mis-parsed on interchange. |

## HIGH — wrong image / wrong metadata / lost operator signals

| ID | File | Defect |
|----|------|--------|
| CR1 | `ndarray_pool.rs:326-331` | `convert` sets `reverse` directly; C++ XORs with input dim's `reverse`. Region extracted from an already-reversed array records wrong orientation. |
| CR3 | `ndarray_pool.rs:160-163` | Pool memory accounting uses `Vec::capacity` added *after* the `max_memory` check → first over-limit alloc succeeds; `release` trims unclamped capacity → underflow panic / counter wrap. `POOL_USED_MEMORY` diverges from C++. |
| TR1 | `transform.rs:13-33` | Transform enum mis-mapped: C++ `Mirror=4, Rot90Mirror=5, Rot180Mirror=6, Rot270Mirror=7`. Rust 5→FlipVert, 6→FlipDiag — transforms 5/6 produce wrong image. |
| BP1 | `bad_pixel.rs` | Median kernel: C++ `[3,3]` is half-extent (7×7); Rust treats as full 3×3. JSON schema fully incompatible with C++ (`{"Bad pixels":[{"Pixel":[x,y],"Set":v}]}` vs Rust's). Existing AD files fail to load. |
| PR1 | `process.rs:737-746` | `AUTO_OFFSET_SCALE` handler is an empty no-op; the correct `auto_offset_scale()` helper is never called — advertised feature dead. |
| PF-B1 | plugin framework | Sort mode buffers **every** array up to `sort_time`; C++ emits in-order arrays immediately. (Integration test wrongly encodes the bug as expected.) |
| PF-G1 | plugin framework | `DroppedArrays` PV never incremented — primary "plugin overloaded" signal lost. |
| PF-G2 | plugin framework | `QueueFree`/`QueueSize` never updated at runtime; dead PVs (+ `QUEUE_FREE` vs `queue_use` name mismatch). |
| F3 | `file_netcdf.rs` | `array_data` omits leading `numArrays` dimension for single-frame files (C++ always rank `ndims+1`); attributes written as static first-frame var-attrs, not per-frame `Attr_<name>` record vars; `epicsTSSec/Nsec` not written. Wire-format incompatible. |
| F4 | `file_hdf5.rs` | SWMR mode silently drops compression — `open_swmr` never builds the filter pipeline. |

## MEDIUM — partial features / silent inaccuracy

Core: `prepare_array` never writes `TIME_STAMP`, `EPICS_TS_SEC/NSEC`, `N_DIMENSIONS`,
`ARRAY_DIMENSIONS`, `DATA_TYPE`, `COLOR_MODE`, `BAYER_PATTERN`, `CODEC`,
`COMPRESSED_SIZE` RBVs (CR-G5/6/7); `convert`/`convert_type` build arrays outside the
pool → counter drift toward `PoolExhausted` (CR-B6/7); `set_shutter` ignores
open/close delays and force-writes `ShutterStatus` (CR-B4); `convert` runs binning on
compressed bytes instead of rejecting (CR-B8); attributes static-only, no
`updateValues()` (CR-G10); many pool/attr-file params declared but unwired, no
`writeInt32/writeOctet` dispatch (CR-G3/9).

Plugin framework: `MaxByteRate`/output throttling absent (G7); `MinCallbackTime`-throttled
arrays dropped with no counter (B5); `EnableCallbacks=0` quiesce not synchronous (B6);
`DisorderedArrays` only counted in sort mode (B4); `NumThreads` multi-worker inert (G4);
`ProcessPlugin` not implemented (G5); `NDArrayAddr` ignored (G6); ColorMode/BayerPattern
inferred from dims not read from attributes (B11); control-plane param `try_send` on
64-slot channel can silently drop at IOC save/restore (B14); `FileLazyOpen` inert (B9).

Compute: `overlay.rs` cross thickness ~2× C++, XOR ellipse leaves holes, `TIMESTAMP_FORMAT`
unapplied, hardcoded 5×7 font; `fft.rs` no power-of-2 zero padding, inverse FFT outputs
magnitude (sign lost); `circular_buff.rs` withholds frames until sequence completes
instead of streaming; `process.rs:479-483` frame-filter forwards unprocessed input
instead of dropping; `roi.rs` offset clamp off-by-one, bin size unclamped, no RGB dim
swap; `scatter.rs` `num_outputs` never param-wired → degenerates to passthrough.

Files: HDF5 layout-XML / NDAttribute datasets / performance dataset / chunking params
all registered but unimplemented; NeXus XML template engine unimplemented (`NX_class`
stored as dataset not group attr → not NeXus-readable); TIFF standard tags
(`Software`/`Model`/`Make`/`ImageDescription`/EPICS-TS) not written; Magick
`COMPRESS_TYPE`/`BIT_DEPTH` no-ops, F32 clamps to [0,1]; JPEG default-quality
inconsistency (IOC 90 / `default()` 50 / C++ 50 / PV seeded 0).

## Feature gaps — registered PVs that do nothing

`DroppedArrays`, `QueueFree`/`QueueSize`, `MaxByteRate`, `NumThreads`/`MaxThreads`,
`NDArrayAddr`, `ProcessPlugin`, `NDDimensions` waveform, `POOL_EMPTY_FREELIST`,
`POOL_POLL_STATS`, `POOL_PRE_ALLOC_BUFFERS`, `CREATE_DIR`, `ND_ATTRIBUTES_FILE/MACROS/STATUS`,
HDF5 chunking/layout/attribute-dataset params, `MAGICK_COMPRESS_TYPE`/`MAGICK_BIT_DEPTH`,
`TIMESTAMP_FORMAT`, `attr_plot` `DataSelect`/`DataLabel`/`NPts`, `codec` Bitshuffle.

## Verified correct (no defect — flagged so future edits preserve)

`Arc<NDArray>` eliminates the entire C refcount bug class. `convert` reverse pixel
*data* is correct (only the metadata flag CR1 is wrong). `QueuedArrayCounter`.
File Single/Capture/Stream/temp-suffix/auto-increment logic. `stats.rs` core
statistics + background subtraction + centroid (computed differently but
mathematically equivalent). `roi_stat.rs` stats. `attribute.rs` extraction.
`gather`/`std_arrays`/`passthrough` passthroughs.

## Why CI is green despite CRITICAL bugs

No test validates against the C ADCore on-disk schema or covers the
dropped-array / throttle / ProcessPlugin paths; tests are Rust-internal
round-trips. The sort-mode test actively encodes bug PF-B1 as expected behavior.
