# ad-core-rs vs ADCore (NDArray/driver core) — Review

> **STATUS (2026-06-28): round-1 RAW review — SUPERSEDED, mostly fixed.**
> The findings below are stated present-tense but are *not* the live open
> list. They were triaged into the dispositioned **ADC-1..ADC-12** inventory
> in `doc/c-parity-review-2026-06-15.md` (round 2), where every item is
> **Fixed / signoff / N-A — zero open**, and closed via the ad-core-rs fix
> stream. Spot-verified fixed in current source: **B1** convert reverse flag
> is now cumulative (`ndarray_pool.rs:515` `reverse ^ src.reverse`); **B6**
> `convert`/`convert_type` now route output through `self.alloc` so it is
> pool-tracked (`ndarray_pool.rs:376,436`); **B8** `convert_type` now rejects
> compressed input (`ndarray_pool.rs:425-428`, C `NDArrayPool.cpp:620-625`);
> **ADC-8** binning sums in the target type (515c1b5c); pool reuse keeps the
> requested `dataSize` (44295f37). Do NOT treat a present-tense finding here as
> open without checking the ADC-N inventory AND current source. The one
> genuine keep-Rust re-design is **G1** NDArray refcount→`Arc<PooledNDArray>`
> (ownership replaces `reserve`/`release`). Kept verbatim for audit provenance.

Scope: NDArray/driver core only (`ndarray*.rs`, `attributes.rs`, `codec.rs`,
`color*.rs`, `pixel_cast.rs`, `roi.rs`, `timestamp.rs`, `driver/*`, `params/*`,
`error.rs`, `runtime.rs`, `lib.rs`). C++ reference: `epics-modules/ADCore/ADApp/ADSrc`.

---

## Feature Gaps

### G1. NDArray reference counting is absent (architectural)
C++ `NDArray` has `referenceCount`, `reserve()`, `release()`, `getReferenceCount()`.
Plugins call `reserve()` on enqueue and `release()` on completion; the pool
returns the buffer to the free list when count hits 0. The Rust port replaces
this with `Arc<PooledNDArray>` (`ndarray_handle.rs`). This is a legitimate
re-design, but note the consequence: the C++ `cantProceed` guards for
`referenceCount < 1` on reserve and `< 0` on release (NDArrayPool.cpp:327,367)
have no Rust equivalent — double-release / use-after-release cannot be detected,
they are simply prevented by ownership. Acceptable, but the pool's
`release(NDArray)` API (ndarray_pool.rs:186) takes an owned array and pushes it
unconditionally to the free list with no ownership check (C++ verifies
`pArray->pNDArrayPool == this`, NDArrayPool.cpp:352). An array allocated from
pool A can be `release`d into pool B, corrupting B's `allocated_bytes`
accounting. No `pNDArrayPool` identity is carried on `NDArray`.

### G2. `NDArrayPool::copy` flags not exposed
C++ `copy(pIn, pOut, copyData, copyDimensions=true, copyDataType=true)` supports
copying into an existing `pOut` and selectively copying data/dims/dataType.
Rust has only `alloc_copy(source)` which always allocates fresh and always
copies everything. `preAllocateBuffers` (C++ uses `copy(pArrays[0], NULL, true)`)
and `readGenericPointer` (`copy(myArray, pArray, 0)` — copy metadata only) have
no Rust path. The `POOL_PRE_ALLOC_BUFFERS` / `POOL_NUM_PRE_ALLOC_BUFFERS`
parameters are created (params/ndarray_driver.rs:134,137) but there is no
`preAllocateBuffers` implementation.

### G3. Pool stats / control parameters created but unwired
`POOL_EMPTY_FREELIST`, `POOL_POLL_STATS`, `POOL_PRE_ALLOC_BUFFERS`,
`POOL_MAX_BUFFERS` are declared in `NDArrayDriverParams` but nothing acts on
them. C++ `asynNDArrayDriver::writeInt32` (asynNDArrayDriver.cpp:684-694)
dispatches: `NDPoolEmptyFreeList → emptyFreeList()`,
`NDPoolPollStats → refresh POOL_USED_MEMORY/ALLOC/FREE`,
`NDPoolPreAllocBuffers → preAllocateBuffers()`. No Rust `writeInt32` handler
exists in the core (`ad_driver.rs` / `ndarray_driver.rs` have no
write-dispatch), so these are dead parameters.

### G4. `getInfo` does not handle YUV color modes
C++ `NDColorMode_t` defines `YUV444/YUV422/YUV411`. `color.rs` defines them and
provides conversion functions, but `NDArray::info()` (ndarray.rs:363-395) only
branches on `RGB1/RGB2/RGB3`; YUV modes fall into the `_` arm and are treated as
plain 3-D (xDim=0,yDim=1,colorDim=2). C++ `getInfo` has the same limitation
(only RGB cases), so this is C-parity — noted as a shared gap, not a Rust
regression.

### G5. Bayer pattern not derived; `BAYER_PATTERN` param dead
C++ reads the `bayerPattern` array attribute to populate `NDBayerPattern`.
`NDBayerPattern` enum exists in `color.rs` but is never read from attributes and
never written to the `BAYER_PATTERN` parameter. `prepare_array` does not set it.

### G6. `compressedSize` / codec params not published from arrays
`NDArray.codec: Option<Codec>` carries `compressed_size`, and params
`CODEC` / `COMPRESSED_SIZE` exist, but `prepare_array`
(ad_driver.rs:125 / ndarray_driver.rs:153) never writes `codec` name or
`compressed_size` to the parameter library. C++ drivers set
`NDCodec`/`NDCompressedSize` from the array. RBV records will stay empty/zero.

### G7. `EPICS_TS_SEC` / `EPICS_TS_NSEC` / `TIME_STAMP` not published
`prepare_array` sets `ARRAY_SIZE_X/Y/Z`, `ARRAY_SIZE`, `UNIQUE_ID` and pool
stats, but never sets `TIME_STAMP`, `EPICS_TS_SEC`, `EPICS_TS_NSEC`,
`N_DIMENSIONS`, `ARRAY_DIMENSIONS`, `DATA_TYPE`, `COLOR_MODE`, `BAYER_PATTERN`.
C++ drivers populate these per array. `NDArray` carries `timestamp` (epicsTS),
`time_stamp` (double) and `dims`, so the data is available but unwired.

### G8. `setIntegerParam(ADAcquire)` AcquireBusy logic absent
C++ overrides `setIntegerParam` (asynNDArrayDriver.cpp:636-663): writing
`ADAcquire=0` drives `ADAcquireBusy` to 0 (gated on `WaitForPlugins` and
`getQueuedArrayCount()==0`); writing `ADAcquire=1` sets `ADAcquireBusy=1`;
`NDNumQueuedArrays=0` while `ADAcquire==0` clears `ADAcquireBusy`. The Rust core
has the `acquire` / `acquire_busy` / `wait_for_plugins` / `num_queued_arrays`
parameters and a `QueuedArrayCounter`, but no equivalent coupling logic in
`ad-core-rs` itself. If a derived driver does not replicate this, AcquireBusy
will not track plugin drain.

### G9. File-handling helpers incomplete
`asynNDArrayDriver` provides `createFilePath` (recursive mkdir up to
`CREATE_DIR` depth), `readNDAttributesFile` (XML attribute loading),
`getAttributes` (`updateValues()` + copy). Rust has `create_file_name` and
`check_path` only. `CREATE_DIR`, `ND_ATTRIBUTES_FILE`, `ND_ATTRIBUTES_MACROS`,
`ND_ATTRIBUTES_STATUS` are dead params. `writeOctet`'s C++ behavior
(`NDFilePath → checkPath → createFilePath on failure`) has no Rust counterpart.

### G10. Attribute types: only static values, no live sources
C++ has `paramAttribute`, `PVAttribute`, `functAttribute` with
`NDAttributeList::updateValues()` that re-reads each attribute from its source
before copying onto an array. `NDAttrSource` in `attributes.rs` enumerates
`Param/EpicsPV/Function/Constant` but `NDAttribute` only stores a static
`value: NDAttrValue` — there is no `update()` / `updateValues()`. Attributes
attached to an array are frozen at attach time; `getAttributes`-style
re-evaluation is not possible. This is a substantial feature gap for the
attribute subsystem.

### G11. `NDArray::report` / `NDArrayPool::report` missing
No diagnostic `report(fp, details)` equivalents. Minor (diagnostics only).

### G12. Codec enum mismatch with C++
`codec.rs::CodecName` = `{None, JPEG, LZ4, Blosc, BSLZ4}`. C++ `NDCodec.h`
codec list is `{NONE, JPEG, BLOSC, LZ4, LZ4HDF5, BSLZ4}` — Rust is missing
`LZ4HDF5` and has no `NDCodecBloscComp_t` (Blosc sub-compressor) support. The
`Codec` struct's `compressor`/`shuffle`/`level` fields exist but the Blosc
compressor name table (`NDCodecBloscCompName`) is absent.

---

## Bugs (ordered by severity)

### B1. HIGH — `NDArrayPool::convert` drops cumulative reverse flag
`ndarray_pool.rs:326-331`. C++ `convert` (NDArrayPool.cpp:719-724):
```c
for (i=0; i<pIn->ndims; i++) {
    pOut->dims[i].offset  = pIn->dims[i].offset + dimsOutCopy[i].offset;
    pOut->dims[i].binning = pIn->dims[i].binning * dimsOutCopy[i].binning;
    if (pIn->dims[i].reverse) pOut->dims[i].reverse = !pOut->dims[i].reverse;
}
```
The output `reverse` is `dimsOut[i].reverse XOR pIn->dims[i].reverse` (cumulative).
The Rust port sets `reverse: dims_out[i].reverse` unconditionally and never XORs
in the source's reverse flag. Consequence: when a region is extracted from an
already-reversed array, the output `NDDimension.reverse` metadata is wrong.
Downstream consumers that interpret `reverse` (orientation relative to the
detector) will mis-orient the region. The pixel data itself is reversed
correctly per `dims_out`, but the recorded orientation is not cumulative.

### B2. HIGH — `convert` reverse extracts the wrong source pixels
`ndarray_pool.rs:363-399`. C++ `convertDim` implements reverse by walking the
*input* with a negative stride starting at the high offset:
`inOffset += pOutDims[dim].size*pOutDims[dim].binning - 1; inDir = -1;`. The bin
window is then summed in input order while the output index advances forward.

The Rust port instead flips the *output coordinate* (`eff_coords[i] =
out_sizes[i]-1-out_coords[i]`) and then reads the bin window at
`offset + eff_coords[i]*bin + bin_off`. For pure reverse (binning=1) this gives
the correct mirrored result and the existing tests (`test_convert_reverse_x/y`)
pass. But when `reverse` and `binning>1` are combined, the bin window is
traversed in the *same* `bin_off` order (0..bin) for reversed and non-reversed
dims. C++ with `inDir=-1` sums the window from high address downward. For a
commutative `+=` sum the *total* is identical, so `test_convert_binning_and_
reverse_combined` passes — **but the offset anchor differs**: C++ anchors the
reversed window at `inOffset = offset + size*binning - 1` and steps the bin
loop with `inc = -inStep`, so element `out=0` aggregates source indices
`[offset + (size-1)*binning .. offset + size*binning - 1]`. The Rust code for
`out_coord=0, reverse` computes `eff=size-1`, then `src = offset + (size-1)*bin
+ bin_off` for `bin_off ∈ 0..bin` → indices `[offset+(size-1)*bin ..
offset+size*bin-1]`. These ranges coincide. So for the binning+reverse case the
results actually match. The genuine divergence is **B1** (metadata) and the
following: the Rust `convert` validation `d.offset + d.size > src.dims[i].size`
(line 311) uses the *unbinned* `d.size`, while the binned C++ uses
`size = size/binning` only for the output-size check and the *offset* bound is
implicit in the pointer walk. The Rust bound is stricter than C++ in one
direction and there is no check that `offset + size*binning` (the actual span
read when reverse anchors at the top) stays in range — but since
`d.offset + d.size <= src.size` and `size` is the pre-divided extent, the span
is `offset + size <= src.size`, which is safe. Net: B2 reduces to the metadata
bug B1; the data is correct. Downgrade B2 to a documentation note — see N3.

### B3. HIGH — Pool memory accounting uses `Vec::capacity`, diverges from C++ and can underflow
`ndarray_pool.rs`. C++ tracks `memorySize_` strictly in `dataSize` units — the
*requested* byte count, added on alloc and subtracted by the same value on free
(NDArrayPool.cpp:212,225,239,776). The Rust pool tracks `allocated_bytes` using
`NDDataBuffer::capacity_bytes()` (`Vec::capacity`), which is allocator-dependent
and almost always larger than the requested size. Two concrete defects:

1. On fresh alloc (lines 140-163) the pool first reserves `needed_bytes` via the
   CAS loop, then *adds* `actual_cap - needed_bytes` afterward (line 160-163)
   — outside the `max_memory` check. A single allocation can therefore push
   `allocated_bytes` past `max_memory` with no error, because the over-shoot is
   added unconditionally after the limit test. C++ never does this; `dataSize`
   is exact.
2. `release` trimming (lines 195-210): `excess` starts at
   `total - max_memory`; each dropped buffer subtracts `dropped_cap.min(total)`
   from `allocated_bytes` but `excess -= dropped_cap` uses the *un-clamped*
   `dropped_cap`. If `dropped_cap > total` (possible because `total` is read
   once and `capacity_bytes` is approximate), the `min` clamps the atomic
   subtraction but `excess` (a `usize`) is decremented by the larger value —
   `excess -= dropped_cap` can underflow-panic in debug or wrap in release.
   The `if dropped_cap >= excess { break; }` guard runs *before* the
   subtraction so it mitigates most cases, but `excess` and the loop condition
   are computed from stale `total`.

Consequence: `POOL_USED_MEMORY` / `POOL_MAX_MEMORY` reported to EPICS will not
match C++ for the same workload (Rust over-reports by capacity slack), and the
`max_memory` limit is not strictly enforced for the first allocation that
exceeds it.

### B4. MEDIUM — `set_shutter` ignores shutter delays and over-writes ShutterStatus
`driver/ad_driver.rs:184-212`. C++ `ADDriver::setShutter` (ADDriver.cpp:29-52):
for `ADShutterModeEPICS` it sets `ADShutterControlEPICS`, calls
`callParamCallbacks()`, then `epicsThreadSleep(shutterOpenDelay -
shutterCloseDelay)`. It does **not** set `ADShutterStatus` in any branch (the
detector/EPICS shutter records drive that). The Rust `set_shutter`:
- never reads `shutter_open_delay` / `shutter_close_delay` and never sleeps —
  the open/close delay is silently dropped (`SHUTTER_OPEN_DELAY` /
  `SHUTTER_CLOSE_DELAY` params are dead);
- unconditionally sets `shutter_status` to the requested state at the end
  (lines 208-209), which C++ does not do — the status should reflect the actual
  shutter, set by the shutter hardware/record, not assumed.
- Handles a `DetectorOnly` branch that writes `SHUTTER_CONTROL`; C++
  `ADShutterModeDetector` is an empty `break` (detector drivers override
  `setShutter` themselves). The Rust behavior is a deviation, though arguably
  convenient for a simulator.

### B5. MEDIUM — `info()` color stride/dim for `Bayer` mode wrong for 3-D, and 2-D path differs
`ndarray.rs:354-396`. For a 2-D array C++ sets `colorSize=0`, `colorStride=0`
(getInfo leaves them at their 0-init); Rust sets `color_size=1`,
`color_stride=0` for the 2-D case (line 361) and `y_size=1`, `color_size=1` for
1-D (line 357). C++ leaves `ySize=0`/`colorSize=0` for 1-D and `colorSize=0` for
2-D. Code that divides by `colorSize` benefits from the Rust `1`, but code that
checks `colorSize==0` to detect "no color dim" (C++ idiom) will misbehave. This
is a defaults divergence, not a crash.

### B6. MEDIUM — `convert` allocates output outside the pool
`ndarray_pool.rs:421-432`. C++ `convert` calls `alloc(...)` for the output, so
the result is pool-tracked and counts against `memorySize_`/`numBuffers_`. The
Rust `convert` builds the output `NDArray` directly with `NDArray { ... }` and
only assigns a `unique_id` from the pool counter — it never goes through
`alloc`, so the output is **not** counted in `allocated_bytes`,
`num_alloc_buffers`, and cannot be reused via the free list. `convert_type` has
the same issue when types differ (line 261 calls `color::convert_data_type`
directly). Pool stats under-report after any `convert`; `release`ing such an
array later *adds* it to the free list and the trim logic will subtract its
capacity from `allocated_bytes` it never contributed → accounting drift toward
underflow (compounds B3).

### B7. MEDIUM — `alloc` free-list reuse: oversized-buffer discard mis-accounts
`ndarray_pool.rs:60-77`. When the best free-list entry is larger than
`needed * 1.5`, the code `swap_remove`s it and subtracts `dropped_cap` from
`allocated_bytes` and decrements `num_alloc_buffers`, then falls through to a
fresh allocation. That is correct *only if* the discarded buffer's capacity was
previously added to `allocated_bytes`. But buffers produced by `convert` /
`convert_type` (see B6) were never added, and buffers reused-and-resized smaller
had their capacity adjusted inconsistently. Combined with B3/B6 the atomic can
drift below 0 (it is `AtomicU64`; `fetch_sub` past 0 wraps to a huge value),
after which `max_memory` checks (`current + needed > max_memory`) always fail →
spurious `PoolExhausted`.

### B8. LOW — `convert` rejects compressed data silently / inconsistently
C++ `convert` explicitly checks `!pIn->codec.empty()` and returns `ND_ERROR`
with a message (NDArrayPool.cpp:620-625). Rust `convert` / `convert_type` do
**not** check `src.codec` — they will happily run binning arithmetic over
compressed bytes, producing garbage. `crop_roi` *does* reject compressed input
(roi.rs:19). Inconsistent and a correctness bug for `convert`.

### B9. LOW — `create_file_name` empty-template fallback diverges from C++
`driver/ndarray_driver.rs:228-233`. When `FILE_TEMPLATE` is empty the Rust code
fabricates `format!("{}{}{:04}", path, name, number)`. C++ passes the empty
template straight to `epicsSnprintf`, yielding an empty string (and the DB
default template is never empty in practice: `"%s%s_%3.3d"`). The Rust fallback
is a non-C behavior; harmless for normal use but not parity.

### B10. LOW — `format_int_spec` width vs precision conflated
`driver/ndarray_driver.rs:69-97`. For C `%3.3d`, width=3 / precision=3 both
mean "at least 3 digits, zero-padded" here so output matches. But for `%5.3d`
(width 5, precision 3) C printf produces `"  042"` (3-digit zero-padded value
right-justified in field 5). The Rust code computes `min_digits =
max(width,precision)=5` and, because `precision>0`, takes the
zero-pad branch → `"00042"`. C++ would give `"  042"`. Edge case; the common
AD templates only use `%Nd` or `%N.Nd` with equal N, so no field impact today.

### B11. LOW — `EpicsTimestamp` cannot represent pre-1990 / has no leap handling
`timestamp.rs:36`. `from(SystemTime)` does `unix_secs.saturating_sub(
EPICS_EPOCH_OFFSET)` — any time before 1990-01-01 saturates to `sec=0` silently.
C++ `epicsTimeStamp` is unsigned `secPastEpoch` too, so this is parity, but the
silent saturation differs from C which would underflow-wrap. Minor.

---

## Notes

### N1. Pixel conversion: clamp+round vs C truncation
`pixel_cast.rs::PixelCast::from_f64` does `v.round().clamp(MIN,MAX)`. C++
`convertType` uses a bare C cast `(dataTypeOut)(*pDataIn++)` — **truncation**,
**no clamp** (signed-overflow is UB in C++ but in practice wraps). `color.rs::
convert_data_type` uses `clamp(...) as T` — clamp but **truncate** (no round).
So three different rounding behaviors coexist:
- `crop_roi` → `PixelCast` → round + clamp
- pool `convert`/`convert_type` → `convert_data_type` → truncate + clamp
- `rgb1_to_mono` → explicit `.round()`
None matches C++ exactly (C++ = truncate, no clamp). The clamping is a
deliberate safety improvement; the round/truncate inconsistency between
`crop_roi` and `convert` is an internal inconsistency worth unifying.

### N2. `NDDataBuffer::as_u8_slice` uses `transmute`-style raw slices
`ndarray.rs:152-181`. `from_raw_parts(v.as_ptr() as *const u8, len*size)` is
sound for reading POD, but exposes native endianness — any wire/file consumer
must be endian-aware. C++ has the same byte-blob assumption. Noted, not a bug.

### N3. `convert` reverse data path is correct
Despite the C++ algorithm walking the input with negative stride vs the Rust
output-coordinate flip, the produced pixel data is equal for all tested
combinations (reverse alone, reverse+binning) because the bin-window sum is
commutative and the anchor ranges coincide. The only real `convert` defect for
reverse is the **metadata** flag — see B1.

### N4. Duplicate `create_param` calls are harmless
`NDArrayDriverParams` creates `ACQUIRE`, `ACQUIRE_BUSY`, `WAIT_FOR_PLUGINS`;
`ADDriverParams` creates the same three again. `param.rs::create_param` (lax)
returns the existing index on a name hit, so both structs' fields resolve to the
same parameter. No duplication in the param library. (`create_param_strict`
would have errored; the core deliberately uses the lax variant.)

### N5. `prepare_array` fires `call_param_callbacks` every frame — matches C++
C++ `writeInt32`/driver loops call `callParamCallbacks()`. The deliberate choice
in `ADDriverBase::new` to NOT fire callbacks at construction (ad_driver.rs:91-100)
is correct given the no-subscribers-before-iocInit constraint and is well
documented.

### N6. `n_dimensions` / `array_dimensions` never populated
`N_DIMENSIONS` (Int32) and `ARRAY_DIMENSIONS` (Int32Array) params exist but
`prepare_array` never writes them from `array.dims`. Folded into G7.
