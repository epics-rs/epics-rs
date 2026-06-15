# Workspace C-parity review — 2026-06-15 (round 2, remaining 8 crates)

Codex-style C→Rust output-form parity audit across the default-member crates
**not** covered by round 1 (`c-parity-review-2026-06-14.md`, which did
base/ca/pva/motor/bridge). Read-only fan-out (6 parallel general-purpose
agents), one category per upstream:

| Crate | Upstream (local) |
|-------|------------------|
| ad-core-rs | `~/codes/epics-modules/ADCore/ADApp/ADSrc` + `pluginSrc/NDPluginDriver.cpp` |
| ad-plugins-rs | `~/codes/epics-modules/ADCore/ADApp/pluginSrc/NDPlugin*.cpp`, `NDFile*.cpp` |
| scaler-rs | `~/codes/epics-modules/scaler/scalerApp/src` (scalerRecord.c, devScaler*) |
| std-rs | `~/codes/epics-modules/std/stdApp/src` (epidRecord.c, throttleRecord.c, timestampRecord.c, devEpid*, devTimeOfDay.c) |
| optics-rs | `~/codes/epics-modules/optics` (tableRecord, orient.c/matrix3.c, kohzu/ml_mono/pf4/qxbpm/Io SNL) |
| modbus-rs | `~/codes/epics-modules/modbus/modbusApp/src` (drvModbusAsyn.cpp, modbusInterpose.c) |
| mqtt-rs | `~/codes/epics-modules/mqtt/mqttSup/src` (drvMqtt.cpp, mqttClient.cpp) |
| epics-tools-rs (procServ) | `~/codes/epics-modules/procServ` (procServ.cc, libtelnet.c, clientFactory.cc, acceptFactory.cc) |

`epics-rs` (umbrella, 1 file) and `epics-macros-rs` (proc-macro, no C upstream)
have no meaningful parity surface and were not audited.

## Parity philosophy (scope filter — same as round 1)

The **only** thing that must match upstream is the **OUTPUT FORM** — wire/byte
format, DBR/PVA encodings, device-protocol bytes (Modbus PDU, MQTT payload,
telnet IAC), on-the-wire field values, externally observable record-field
outputs, and monitor-post shape/mask. Internal design may differ or improve as
long as it is functionally equivalent; a design deviation that produces
*identical* observable output is **not** a finding. Every finding names a
concrete observable consequence.

Finding IDs are used **only in this doc** (not in source comments or commit
messages). Commits cite the C reference + rationale.

## Disposition legend

- **fix** — clear output-form divergence, fix to match upstream.
- **fix-low** — real divergence, narrow or last-digit/packet-level impact; fix for completeness.
- **signoff** — output differs, but the Rust behavior is an intentional improvement / correction of a latent C defect, or closing it needs an architecture/semantic change; surfaced for user decision rather than silently changed.
- **verify** — reachability or premise uncertain; confirm at file:line on both sides before fixing.

## Verification status

The audit was a parallel read-only fan-out. The owning session re-verified the
highest-impact items personally (the agents' notes mark which). Findings tagged
**verify**, and the optics table-record candidates (T-1..T-6), were NOT
independently confirmed on both sides — confirm before editing, per the
"defect citation is a sample" rule.

## Resolution status (round 2 — closing 2026-06-15)

Each finding re-verified at file:line on BOTH the C and Rust sides before
editing (the audit was a read-only fan-out; citations are samples, not the
population). One commit per finding.

| Finding | Disposition | Status | Commit |
|---------|-------------|--------|--------|
| STD-1/2/3 (epid OUTL-write gating) | fix | Fixed | 818148c7 |
| STD-4 (epid MLST/ALST double-advance) | fix | Fixed | 1e79cc76 |
| STD-5 (timestamp `.%03f` round vs truncate) | fix-low | Fixed | 4adf6ec8 |
| STD-6 (timestamp VAL posts every cycle) | fix | Fixed | 4e3d4990 |
| SCAL-3 (arm(0) disarm clears counts) | fix | Fixed | b83b8af1 |
| SCAL-1 (idle S1..Sn DBE_LOG sweep) | fix | Fixed | 3486badf |
| OPT-1 (orient Mode constraint 1/2 swap) | fix | Fixed | 0d744983 |
| OPT-2 (singular A0/OMTX publishes stale, C identity) | fix | Fixed | 8f5bec53 |
| OPT-4 (kohzu soft-limit setpoint revert) | fix | Fixed | 1f3cf86f |
| OPT-5 (kohzu/ml-mono tweak inc/dec feature) | fix | Fixed | 7e852463 |
| OPT-6 (kohzu/ml-mono forbidden-reflection Alert flag) | fix | Fixed | 5888a507 |
| OPT-7 (ml-mono standalone Y move retracks Z) | fix | Fixed | c73488ad |
| OPT-8 (PF4 Al/Ti/Glass analytic absorption fits) | fix | Fixed | 543cc7e3 |
| OPT-9 (PF4 filterAl/Ti/Gl material+bank gate) | fix-low | Fixed | 5baac515 |
| OPT-10 (flexCombinedMotion give-up extra {FM}.VAL write) | fix | Fixed | 50b2c6cd |
| OPT-11 (QXBPM set_defaults preserves offsets) | fix | Fixed | 4fc2e166 |
| OPT-12 (QXBPM pos:x/y unguarded divide → NaN/Inf) | verify→fix | Fixed | 3eeda648 |
| OPT-16 (PF4 invTrans gated on trans>0) | fix | Fixed | de2beb9f |
| OPT-14 (Io scaler.DESC = selected channel name) | fix | Fixed | 18a2795b |
| OPT-15a/b (Io out-of-range channel + zero-ticks ionAbs) | fix-low | Fixed | 67fa074b |
| OPT-15c (Io 6-sig-fig absorption coefficients) | fix-low | Fixed | 01d9fc5d |
| OPT-13 (Io init force-writes 19 default PVs) | fork→B | Fixed (option B) | 7e968506 |
| OPT-T1 (table YANG put rotates user offsets) | verify→fix | Fixed | 7b443c58 |
| OPT-T2 (table restores speed on every speed-capable motor) | verify→fix | Fixed | d7318d2d |
| OPT-T3 (table zeroes motor limits on read failure) | verify→fix | Fixed | 9dc07384 |
| OPT-T4 (Newport user limits use raw-angle rotation matrix) | verify→fix | Fixed | c82703ec |
| OPT-T5 (table sqrt/asin domain clamps vs C bare NaN/Inf) | signoff | Resolved — keep Rust guards (user 2026-06-15) | — |
| OPT-T6 (table speed-ratio NaN guard vs C 0/0 poison) | signoff | Resolved — keep Rust guard (user 2026-06-15) | — |
| OPT-3 (orient invertArray x/det vs x*(1/det) de-precision) | signoff | Resolved — keep Rust precision (user 2026-06-15) | — |
| MQTT-1 (FLAT inbound INT/FLOAT/DIGITAL parses raw, rejects surrounding ws) | fix | Fixed | cb9ba4e9 |
| MQTT-2 (FLAT:STRING inbound stored verbatim) | fix | Fixed | db0fc076 |
| MQTT-4 (octet value terminates at first NUL) | fix-low | Fixed | 36f96ca1 |
| PROC-1 (telnet RFC1143 negotiation, not blanket option refusal) | fix | Fixed | 40770a28 |
| PROC-2 (info file/PROCSERV_INFO manage-procs format) | fix | Fixed | 7bf565d8 |
| PROC-3 (procServ branding/version string) | signoff | Signoff — keep Rust branding (user call) | — |
| ADC-6 / ADP-2 (RGB→Mono `(R+G+B)/3` truncated, not luminance+round) | fix | Fixed | 1273b533 |
| ADC-4 (MinCallbackTime-throttled frame must not count as dropped) | fix | Fixed | a1cb7e0c |
| ADC-1 (PARAM NDAttribute type follows configured `datatype`, not runtime type) | verify→fix | Fixed (option: honor datatype) | eecf79c8 |
| ADC-2 (`NDArrayCallbacks=0` must stop downstream NDArray delivery, plugin path) | fix | Fixed | cf59bf78 |
| ADC-3 (plugin output must publish NDCodec / NDCompressedSize per array) | fix | Fixed | f3f44a39 |
| ADC-5 (NDDimensions posts fixed ND_ARRAY_MAX_DIMS=10 zero-filled, not ndims) | fix-low | Fixed | 600adb66 |
| ADC-11 (file-plugin control attrs honor C string-typed read, ignore numeric) | verify→fix | Fixed | bc63c38f |
| ADC-12 (destination_matches comparison must replicate C attrIsProcessingRequired: non-empty guard + "all" 3-char prefix) | fix-low | Fixed | a05b900c |
| ADP-10 (overlay shape ordinals Text=2/Ellipse=3) | fix | Fixed | 7f0d95c4 |
| ADP-20 (overlay Cross independent SizeX/SizeY arms) | fix | Fixed | 314b5912 |
| ADP-21 (overlay Rectangle inclusive bounds, SizeX+1 wide) | fix | Fixed | ad15297a |
| ADP-22 (overlay text skips codes ≥128, C signed-char rule) | fix | Fixed | 5d767953 |
| ADP-11 (COMPRESSOR ordinal Blosc=2/LZ4=3/BSLZ4=4; zlib/lz4hdf5→5/6) | fix | Fixed | 39e07250 |
| ADP-29 (Blosc default clevel 5; record real codec params) | fix-low | Fixed | e92d42be |
| ADP-5 (process clip order high-then-low) | fix | Fixed | b2eea48b |
| ADP-24 (flat-field uses scaleFlatField directly, no mean substitution) | fix | Fixed | bbd7fd5e |
| ADP-7 (valid bg/flat-field recomputed per frame; size mismatch invalidates) | fix | Fixed | afead50f |
| ADP-6 (auto offset/scale arms next frame, not the trigger frame) | fix | Fixed | 570965ea |
| ADP-8 (TimeSeries integer truncation before dividing) | fix→N/A | Not applicable (Rust accumulate fed only f64 stats = C NDFloat64 path; raw-array TS unimplemented) | — |
| ADP-14 (stats clamps out-of-range cursor/centroid to edge, not zeros) | fix | Fixed | b1b6f336 |
| ADP-13 (stats skips centroid/profiles/cursor for ndims>2) | fix | Fixed | 6cfb6d1e |
| ADP-15 (stats histogram upper-boundary clamp) | fix→N/A | Not applicable (clamp is a no-op for in-range values; guards already match C; proof in finding block) | — |
| ADP-1 (stats centroid moments: raw-pixel vs threshold-profile) | fix→signoff | Resolved — keep Rust direct-central moments, decoded-equivalent precision fork (user 2026-06-15) | — |
| ADP-3 (false-color Rainbow/Iron LUTs by index, not generated jet) | fix | Fixed | 81d90f28 |
| ADP-4 (Bayer demosaic border keeps native channel only) | fix | Fixed | 3b1669c6 |
| ADP-9 (FFT rank from input ndims, not a fixed mode; 2-D input → full 2-D FFT) | fix | Fixed | 7943d427 |
| ADP-25 (FFT TimeSeries/TimeAxis posted at padded nTimeX) | fix | Fixed | 61b9964d |
| ADP-12 (JPEG RGB2/RGB3 → RGB; convert_rgb_layout tags output ColorMode) | fix | Fixed | 487d3bd4 |
| ADP-17 (ROIStat dispatches stats by array rank; 1-D background sums only the two X-end strips) | fix | Fixed | a8ac8287 |
| ADP-16 (ROIStat clamps out-of-range/zero ROI to one edge pixel; writes back clamped geometry) | fix | Fixed | 017d3497 |
| ADP-18 (ROI 3-D RGB path converts output to the requested dataType) | fix | Fixed | 3064f514 |
| ADP-19 (ROI single-color selection collapses to 2-D Mono) | fix | Fixed | c87ac8c3 |
| ADP-23 (transform layout: array attr vs NDColorMode param) | verify→N/A | Not applicable (C refreshes the param from the array ColorMode attr each frame before use; same Mono default — equivalent, mismatch unreachable) | — |
| ADP-27 (netCDF global-attr set: add NDNetCDFFileVersion=3.1; drop extra uniqueId/numArrays) | fix | Fixed | dbe09fd4 |
| ADP-28 (TIFF RGB2/RGB3 PlanarConfig=SEPARATE vs Rust chunky-RGB1) | fix→signoff | Resolved — keep Rust chunky-RGB1, decoded-image-equivalent (user 2026-06-15) | — |
| ADP-30a (Stats HIST_BELOW/HIST_ABOVE Int32 param, not Float64) | fix-low | Fixed | 064b8011 |
| ADP-30b (TIFF extra IFD tags / RowsPerStrip≠height) | signoff | Resolved — keep Rust IFD tags, decoded-equivalent (user 2026-06-15) | — |
| STD-7/8 (time_of_day record-timestamp source + `<undefined>` sentinel) | signoff→fix | Fixed (user: Match C) | c079c35e |
| ADC-8 (pool.convert binning sums in the target type, C-exact wrap/widen/precision) | verify→fix | Fixed (user: fix-now) | 515c1b5c |

STD-1/2/3 share one structural root (single-owner OUTL-write flag set only by
`do_pid`), so they land in one commit. STD-7/8 were re-dispositioned from
signoff to fix (user: "Match C") and landed together in c079c35e: a single
`recgbl::get_time_stamp` owner shared with `apply_timestamp`, the TSE-resolved
stamp formatted by both device supports, and the `<undefined>` epoch sentinel.

SCAL-1 added a `Record::log_swept_fields()` hook (LOG-only analogue of
`force_posted_fields`), wired into the four monitor-snapshot builders — same
monitor-post-fidelity family as round-1 MOT-1. SCAL-2 (fix-low) is the
residual completion-cycle VALUE|LOG packet-bundling, left open; SCAL-4/5 are
signoff (see tally).

## Open Findings

### ad-core-rs (ADC)

#### ADC-1: PARAM-type NDAttribute publishes the param's runtime type, not the configured `datatype` (default int) — FIXED eecf79c8
Severity: High — fix
Re-verification (2026-06-15): the finding's stated impact is imprecise — C's omitted-`datatype` default is the lower-case `"int"` (asynNDArrayDriver.cpp:446), which matches NONE of the upper-case `strcmp` branches (paramAttribute.cpp:80-95) → `paramAttrTypeUnknown` → the attribute is NDAttrUndefined and never updated, NOT an `NDAttrInt32` truncated int. For every schema-valid config (datatype == the param's actual type, documented as required at :297) the Rust runtime-type derivation already produced byte-identical output; divergence existed only on out-of-schema input (omitted/mismatched datatype). User chose "honor datatype (parity)": parse `datatype` → `ParamAttrType`, dispatch the getter by it, leave Unknown / wrong-type reads Undefined (matching C's never-refreshed path). The typed-zero C writes on a wrong-type read (documented misconfig) is intentionally not reproduced.
Rust: `crates/ad-core-rs/src/driver/ndarray_driver.rs:92-107` (`parse_attributes_xml` PARAM branch) ignores the XML `datatype`; `:783-814` (`read_param_value`) derives the published `NDAttrValue` from the param's runtime type.
C: `ADApp/ADSrc/asynNDArrayDriver.cpp:445-446` reads `datatype` (default `"int"`); `paramAttribute.cpp:80-95,131-151` maps it to a fixed NDAttr type and reads the param accordingly.
Impact: `<Attribute type="PARAM" source="GAIN"/>` on a Float64 param publishes, in C, an NDAttrInt32 (code 4) holding the truncated integer (default `datatype="int"`); Rust publishes NDAttrFloat64 (code 9) with the float. NTNDArray/file attribute carries a different dataType code AND value.

#### ADC-2: `NDArrayCallbacks=0` does not stop downstream NDArray delivery (plugin path) — FIXED cf59bf78
Severity: High — fix
Fix: `SharedProcessorInner.array_callbacks` (updated from the ARRAY_CALLBACKS param write) gates the two downstream-delivery mechanisms at the single owner — `build_publish_batch(deliver)` skips the STD_ARRAY_DATA generic-pointer interrupt and empties `ProcessOutput.arrays`, and `process_and_publish` skips `route_output_arrays` (no throttle/sort admission) — while still publishing the begin metadata params. Matches C `endProcessCallbacks`:257-265. Sort-buffer flushes always deliver (admitted while on; C sort thread is flag-independent). Distinct from `enabled` (EnableCallbacks).
Rust: `crates/ad-core-rs/src/plugin/runtime.rs:505,928` always emits the output array; `array_callbacks` (`:1022`) is never read in the data loop (only `enabled` gates output).
C: `ADApp/pluginSrc/NDPluginDriver.cpp:257-265` — `endProcessCallbacks` caches the array and returns without `doCallbacksGenericPointer` when `NDArrayCallbacks==0`.
Impact: with `ArrayCallbacks=0`, C withholds NDArrays from downstream; Rust keeps publishing every frame. (Driver-base path `driver/ndarray_driver.rs:523-537` honors the gate; only the plugin runtime diverges.)

#### ADC-3: Plugin output array does not publish NDCodec / NDCompressedSize params — FIXED f3f44a39
Severity: Medium — fix
Rust: `crates/ad-core-rs/src/plugin/runtime.rs:759-825` sets the standard params but never `codec`/`compressed_size` (both exist, `params/ndarray_driver.rs:119-120`).
C: `NDPluginDriver.cpp:213-214` — `beginProcessCallbacks` sets `NDCodec`=codec name and `NDCompressedSize` on every array.
Impact: a caget on a plugin's `Codec_RBV`/`CompressedSize_RBV` never updates in Rust; in C they track each array. (Driver-base path publishes them.)

#### ADC-4: MinCallbackTime-throttled frame wrongly increments DroppedArrays — FIXED a1cb7e0c
Severity: Medium — fix
Rust: `crates/ad-core-rs/src/plugin/runtime.rs:539-543` — a MinCallbackTime-throttled frame does `dropped_arrays.fetch_add(1)`.
C: `NDPluginDriver.cpp:405-449` — a `deltaTime <= minCallbackTime` frame skips to `callParamCallbacks()`; `droppedArrays++` only on queue-full (`:440`) and compression-not-aware (`:388`).
Impact: with nonzero MinCallbackTime under fast input, Rust's `DroppedArrays_RBV` over-counts vs C.
Fix: `process_and_publish` throttle path returns `None` (no param post) and no longer increments `dropped_arrays`; the surviving increments (compression-unaware runtime path, queue-full channel.rs) match C:388/:440. Test `test_min_callback_time_throttle_not_counted` asserts DroppedArrays stays 0 after a throttled frame.

#### ADC-5: NDDimensions int32-array post carries `ndims` elements, not `ND_ARRAY_MAX_DIMS` (10) — FIXED 600adb66
Severity: Medium — fix-low
Fix: new `ND_ARRAY_MAX_DIMS=10` constant; both posting sites (driver-base `write_array_params`, plugin runtime G8 interrupt) now build a 10-element zero-filled array, and the plugin `dims_prev` seed is zero-filled length-10 to match C's `dimsPrev_` change-detection.
Rust: `crates/ad-core-rs/src/plugin/runtime.rs:742-757` (and `driver/ndarray_driver.rs:281-284`) post a length-`ndims` int32 array.
C: `NDPluginDriver.cpp:221-231` posts `dimsPrev_[ND_ARRAY_MAX_DIMS]` (zero-filled) → `readInt32Array` returns 10.
Impact: caget on `Dimensions_RBV` reads NORD=ndims in Rust vs 10 (trailing zeros) in C.

#### ADC-6: RGB1→Mono uses luminance weights + round; C uses unweighted `(R+G+B)/3` truncated — SAME CODE AS ADP-2 — FIXED 1273b533
Severity: High (wired via ad-plugins) — fix
Rust: `crates/ad-core-rs/src/color.rs:114-160` (`rgb1_to_mono`) `0.299R+0.587G+0.114B` then `.round()`. Round-2 found this unwired *inside ad-core*, but **ad-plugins `color_convert.rs:437` wires it** (see ADP-2) — so it is reachable and High. Fix once, in `color.rs`.
C: `NDPluginColorConvert.cpp:392-396` `value=(R+G+B)/3.` then `(epicsType)value`.
Impact: every non-gray RGB→mono pixel differs (R=255,G=0,B=0 → C 85, Rust 76).

#### ADC-7: ad-core implements YUV conversions ADCore never performs
Severity: Low — signoff
Rust: `crates/ad-core-rs/src/color.rs:373-777` (rgb↔yuv444/422/411).
C: `NDPluginColorConvert.cpp` handles only Mono↔RGB1/2/3 + Bayer; no YUV anywhere in ADCore.
Impact: no divergence today (unwired). Conversely the C Bayer→RGB demosaic has a Rust counterpart only in ad-plugins (ADP-4). Flagged so the YUV paths are not mistaken for a faithful port.
RESOLVED 2026-06-15 — keep the extra YUV paths (user): additive, unwired, no output-form divergence.

#### ADC-8: `pool.convert` binning sums in f64 then casts once; C casts each element to the output type and accumulates there — FIXED 515c1b5c
Severity: Low — verify (unwired: wired ROI does pure cropping, no binning)
Rust: `crates/ad-core-rs/src/ndarray_pool.rs:516-543` accumulates in f64 then `out=sum as T` (saturates).
C: `NDArrayPool.cpp:460-466` `*pDOut += (dataTypeOut)*pDIn` (output-type arithmetic, integer wrap).
Impact: a binning sum past the int range — C wraps, Rust saturates. Latent.

VERIFIED 2026-06-15 — CONFIRMED REAL, production-unwired → surfaced for decision (#58). The Rust binning loop accumulates in `f64` and casts to the **source** type (`out[out_idx] = sum as $T` where `$T` = source type, ndarray_pool.rs:516-543), then a separate `convert_data_type` step (`:573-581`) casts to the target type. C instead accumulates directly in the **output** type (`*pDOut += (dataTypeOut)*pDIn`). Three divergences result when binning>1: (a) integer overflow — C wraps, Rust saturates; (b) **widening target** (e.g. u8 binned then converted to u16) — Rust clamps the bin sum to the *source* (u8) range before widening, so 4×100=400 → C 400 vs Rust 255; (c) i64/u64 values > 2^53 lose precision through the f64 round-trip even at binning=1. BUT no wired plugin reaches `NDArrayPool::convert` with binning>1 — the binning>1 sites are all in-crate tests (ndarray_pool.rs:982-1332); the wired ROI path does pure cropping (no binning). The faithful fix (accumulate in the output type with wrapping integer arithmetic across all source/target pairs) is a hot-loop rewrite of the convert macro and touches existing saturation-asserting tests, so per the "confirm before sprawling into a large structural change" rule it is surfaced rather than silently rewritten. Decision: fix-now (lock in C parity at the convert owner before binning is ever wired) vs defer-until-wired.

FIXED 515c1b5c (user: fix-now) — the convert binning loop now accumulates directly in the target type via a `BinAcc` accumulator (i128 for integer targets, the target float type for float targets) and casts the accumulator once to the target element type, dropping the source-typed intermediate + separate `convert_data_type` step. The dispatch is now source-type × target-type so the per-element `(dataTypeOut)` cast happens in the output type, exactly like C `convertDim`. i128 + final narrowing cast reproduces C's per-step integer wrap by the `Z → Z/2^width` homomorphism. Regression tests: widening (u8×4→u16 = 800 not 255), overflow (u8×4 100s → 144 not 255), and i64 2^53+1 exact at binning==1 (the wired-path bug the f64 round-trip caused even for source==target).

#### ADC-9: `pool.convert` rejects offset+size overrun; C does not bound-check
Severity: Low — verify (unwired)
Rust: `crates/ad-core-rs/src/ndarray_pool.rs:450-455` returns `InvalidDimensions`.
C: `NDArrayPool.cpp:602-737` validates only `size/binning>0`; reads past the region.
Impact: error-vs-output, latent.

VERIFIED 2026-06-15 — N/A (Rust is the safer side; do not replicate). C does not bound-check `offset+size` against the source dimension and reads past the array region — a latent out-of-bounds read on the C side. Rust returns a clean `InvalidDimensions` error instead. Matching C's "output form" here would mean deliberately introducing an OOB read (undefined behavior) into safe Rust to reproduce whatever garbage/crash C yields — that is a reference-side defect, not a parity target (cf. port-translation-lessons: the audit does not replicate C bugs). The bound-check stays; no code change.

#### ADC-10: `CodecName` enum carries `Zlib`/`LZ4HDF5` not in C `codecName[]`, with a different ordinal order — STRUCTURAL CAUSE of ADP-11
Severity: Low (enum) / High (the ordinal shift it causes, see ADP-11) — fix (fold into ADP-11)
Rust: `crates/ad-core-rs/src/codec.rs:5-31` — 7 variants; `as_str` emits `"zlib"`/`"lz4hdf5"`.
C: `ADApp/ADSrc/Codec.h:4-18` — `{"","jpeg","blosc","lz4","bslz4"}`, `NDCODEC_{NONE=0,JPEG=1,BLOSC=2,LZ4=3,BSLZ4=4}`.
Impact: the four real names round-trip; the extra variants + the ad-plugins ordinal map (ADP-11) cause `COMPRESSOR=2/3/4` to select the wrong codec.

#### ADC-11: file-name NDAttribute path stringifies numeric attributes; C `getValue(NDAttrString)` errors and ignores them — FIXED bc63c38f
Severity: Low — verify
Rust: `crates/ad-core-rs/src/plugin/file_controller.rs:203,244`, `plugin/file_base.rs:239` use `as_string()` (renders numeric → decimal).
C: `NDPluginFile.cpp:548,382` call `getValue(NDAttrString,…)`; `NDAttribute.cpp:349-361` returns ND_ERROR for a non-string attribute.
Impact: a misconfigured numeric filename attribute changes the output filename in Rust, ignored in C. Edge (non-conformant typing).
Fix: new `NDAttrValue::as_string_typed() -> Option<&str>` (Some only for the String variant, mirroring `getValue(NDAttrString)`); all three control-read sites (FilePluginDestination, FilePluginFileName, DriverFileName) route through it so numeric/undefined attributes are ignored as in C. Serialization sites (`NDArray::report`, NeXus/TIFF/HDF5 writers) keep `as_string()` — C stringifies for storage there too (distinct, not in family).

#### ADC-12: `destination_matches` comparison diverges from C `attrIsProcessingRequired` (length guard + "all" prefix) — FIXED a05b900c
Severity: Low — fix-low. Discovered while fixing ADC-11; distinct root cause (comparison semantics, not numeric stringification).
Rust: `crates/ad-core-rs/src/plugin/file_controller.rs` `destination_matches` — `if dest.len() <= 1 { return true }` skips the compare for a 1-char destination, and `dest.eq_ignore_ascii_case("all")` is a full-string equality.
C: `NDPluginFile.cpp:639-648` runs the compare whenever the attr is string-typed and non-empty (`getValueInfo` size = `strlen+1 > 1`, so size ≥ 1 / non-empty), tests "all" via `epicsStrnCaseCmp(dest,"all",min(len,3))` (a 3-char prefix match), and the port name via full-length compare.
Impact: a 1-char destination port is always processed in Rust but compared in C; a destination like `allfoo` matches the "all" prefix in C (processed) but neither "all" nor the port in Rust (skipped). Edge (degenerate/short port names), but observable routing differs.

Clean in ad-core (verified): `ndarray.rs` getInfo layout, pool alloc/release/free-list/THRESHOLD 1.5, attributes source mapping + copy_from, pixel_cast round+clamp, color_layout, timestamp epoch offset; runtime queue-full/compression-drop/QueueFree/MaxByteRate/ArrayCounter/ColorMode-BayerPattern/SortBuffer.

### ad-plugins-rs (ADP)

#### ADP-1: Stats centroid higher moments from raw 2-D pixels, not threshold projection profiles
Severity: High — fix → SIGNOFF (precision-only fork; recommend keep Rust, per OPT-3 precedent)
Rust: `crates/ad-plugins-rs/src/stats.rs:493-524` accumulates mu20/mu02/mu11/m30..m04 per raw pixel.
C: `NDPluginStats.cpp:224-241` — M20/M30/M40 from `profileX[profThreshold]`, M02/M03/M04 from `profileY[profThreshold]`; only M11 (`:215`) is a raw cross-sum.
Impact: SIGMAXY/ECCENTRICITY/ORIENTATION diverge even at threshold 0; all marginal moments diverge for centroidThreshold>0.
— RE-VERIFIED — NOT an algorithmic divergence; it is a floating-point precision fork. The two methods are mathematically IDENTICAL: C's `profileX[profThreshold][ix]=Σ_iy value` then `M20=Σ_ix profileX_thresh[ix]·ix²` equals `Σ_thresholded-pixels value·ix²` because the moment weight `ix^k`/`iy^k` is separable and the threshold is applied PER PIXEL in both (so the finding's "diverge for threshold>0" premise does not hold — the per-pixel threshold preserves separability). Both reduce to the same central moments: C computes raw moments then `μ20=M20−M10²/M00` (and the full 3rd/4th-order expansions at :248-253), which equals `Σv·(ix−cx)²` — exactly Rust's direct form (verified the μ3/μ4 expansions algebraically). Every downstream formula (sigmaX=√(μ20/M00), sigmaXY=varXY/(σX·σY), skew=μ30/(M00·varX^1.5), kurtosis=μ40/(M00·varX²)−3, eccentricity, orientation) is byte-identical between the two. The ONLY difference is fp rounding: C's `M20−M10²/M00` suffers catastrophic cancellation once the raw moments exceed 2^53 (reachable only for large/bright images, e.g. 4096² UInt16: M20≈1e19 > 9e15), where Rust's direct `Σv·(ix−cx)²` is numerically STABLE and MORE accurate. For realistic integer image data with moments ≤ 2^53 the two are bit-identical. This is the same class as OPT-3 (kept Rust precision, user call) — a numerical-method decision for the user, not a silent rewrite that would deliberately reintroduce C's cancellation. Recommend KEEP Rust's stable direct-central method. No code change pending sign-off. Routed to the #58 signoff batch.

#### ADP-2: ColorConvert RGB→Mono luminance vs `(R+G+B)/3` (= ADC-6, fix in color.rs) — FIXED 1273b533
Severity: High — fix
Rust: `crates/ad-core-rs/src/color.rs:131-132`; wired by `color_convert.rs:437`.
C: `NDPluginColorConvert.cpp:393,462,533`.
Impact: every non-gray RGB→Mono pixel differs.

#### ADP-3: ColorConvert false-color uses a generated jet LUT, not Rainbow/Iron tables — FIXED (81d90f28)
Severity: High — fix
Rust: `crates/ad-plugins-rs/src/color_convert.rs:270-281,414-418` — any nonzero falseColor → jet table (index 0 → (0,0,127)); 1-vs-2 ignored.
C: `NDPluginColorConvert.cpp:62-77` selects RainbowColor (1) / IronColor (2) from `colorMaps.h`; Rainbow[0]=(0,0,0).
Impact: every false-color output pixel differs; Iron mode is not distinct.
FIXED: ported the exact `RAINBOW_COLOR_MAP`/`IRON_COLOR_MAP` 256-entry tables verbatim from `colorMaps.h`; `false_color_lut(false_color)` selects 1=Rainbow, 2=Iron, else None (→ plain mono→RGB1, matching C's `default: falseColor=0`). Verified colorMapRGB interleaved == separate R/G/B channels, so mono→RGB1 + layout repack equals C's per-channel RGB2/RGB3 application. C only consults the LUT for NDInt8/NDUInt8 input (`int falseColor=0` at line 45 stays 0 otherwise); the Rust UInt8-only restriction matches except the NDInt8-signed path — see ADP-3-note. Tests: test_false_color_conversion (Rainbow), test_false_color_iron_table (Iron), test_false_color_table_endpoints.

> ADP-3-note (distinct, deferred): C applies false color for both NDInt8 and NDUInt8 (cast `(unsigned char)*pIn`). Rust `false_color_mono_to_rgb1` gates on `NDDataType::UInt8` only. Whether NDInt8 mono is reachable through this crate's pipeline needs a separate check before flagging; not part of the ADP-3 LUT fix.

#### ADP-4: ColorConvert Bayer demosaic interpolates the image border; C leaves non-native channels 0 — FIXED (3b1669c6)
Severity: High — fix
Rust: `crates/ad-plugins-rs/src/color_convert.rs:51-191` interpolates every pixel incl. the 1-px border.
C: `NDPluginColorConvert.cpp:305` gates interpolation on interior; border keeps 2 channels at 0.
Impact: the one-pixel border of every demosaiced RGB output differs.
FIXED: gated all non-native interpolation on `interior = x>0 && x+1<w && y>0 && y+1<h` (= C line 305). Border pixels keep only the native Bayer channel (other two stay 0, vecs zero-initialised). Interior pixels always have all 8 neighbours, so the count-based edge divisor (the divergence source) is replaced by C's fixed /4 (red/blue arms) and /2 (green) — one uniform rule, interior output unchanged. Single demosaic site (rg `demosaic` workspace-wide). Tests: test_adp4_bayer_border_keeps_native_channel_only, test_adp4_bayer_interior_uses_fixed_quarter_divisor.

#### ADP-5: Process clip order reversed (C high-then-low, Rust low-then-high)
Severity: High — fix
Rust: `crates/ad-plugins-rs/src/process.rs:409-413` low-clip then high-clip.
C: `NDPluginProcess.cpp:175-176` high-clip then low-clip.
Impact: when the two thresholds cross, per-pixel output differs (v=200, high=100→10, low=50→999: C 999, Rust 10).
— FIXED b2eea48b: apply_stages now clips high-then-low; test test_adp5_clip_order_high_before_low.

#### ADP-6: Process auto-offset-scale transforms the trigger frame; C only measures it
Severity: High — fix
Rust: `crates/ad-plugins-rs/src/process.rs:348-351` runs auto_offset_scale at stage 0b and scales the same frame.
C: `NDPluginProcess.cpp:164-178,238-249` outputs raw that frame, applies new scaling from the next.
Impact: the trigger frame's whole array diverges.
— FIXED 570965ea: stage 0b consumes the one-shot, arming deferred to after the output array is built (skipped on suppressed frames, matching pArrayOut==NULL); test test_adp6_auto_offset_scale_arms_next_frame_not_trigger.

#### ADP-7: Process valid background/flat-field never invalidated on element-count change
Severity: High — fix
Rust: `crates/ad-plugins-rs/src/process.rs:160,171,394,400,601-620` keeps valid_*=true permanently; applies over `min(n, bg.len())`.
C: `NDPluginProcess.cpp:120-130` recomputes validBackground/validFlatField from `nElements==nBackgroundElements` each frame and NULLs the pointer on mismatch.
Impact: after an input-size change, C posts VALID_*=0 and skips; Rust posts 1 and applies a partial-prefix op. Status params and array diverge.
— FIXED afead50f: process() recomputes valid_background/valid_flat_field each frame from buffer.len()==n; bg/ff gated on enable&&valid; partial-prefix `i<len` guards removed (length now guaranteed); test test_adp7_size_mismatched_background_invalidated_not_partial. save_*=true at save time retained (matches C writeInt32 ValidBackground=1).

#### ADP-8: TimeSeries per-point average kept as f64; C truncates to the integer element type before dividing
Severity: High — fix
Rust: `crates/ad-plugins-rs/src/time_series.rs:270-274` `average_store[i]/divisor` in f64.
C: `NDPluginTimeSeries.cpp:191` `(epicsType)averageStore_[signal]/numAveraged_`.
Impact: integer source, numAverage>1: UInt8 200,200,200 → C 29, Rust 200. Waveform values diverge (C can wrap).
— NOT APPLICABLE (re-verified, no code change): the Rust generic `accumulate` (SharedTsState) is fed exclusively by Stats/ROIStat/Attribute, all of which send computed **f64** values (`TimeSeriesData.values: Vec<f64>`). This is the exact analogue of the C data flow: `NDPluginStats.cpp:549` allocates the per-frame time-series array as `NDFloat64` (all 23 stats stored as doubles) and feeds it to a downstream NDPluginTimeSeries — so `epicsType==epicsFloat64` there and C's `(epicsFloat64)sum/numAveraged_` is **float** division, no truncation. The integer-truncation case at `:191` only fires when NDPluginTimeSeries ingests a raw integer NDArray directly (`NDTimeSeriesConfigure` on a detector port). The Rust TS port can only attach to a pre-registered receiver (ioc.rs:516 `tsr.take`), and only Stats/Attr/ROIStat register one — there is no raw-array TS plugin (`time_series.rs` has no `process_array`). So the truncation path is unreachable; for every path the port implements, C also divides in f64 → output matches. The unimplemented raw-array-ingestion configuration is a missing-feature gap (a whole separate plugin), not this truncation finding; left for a follow-up round, not silently scope-expanded here.

#### ADP-9: FFT processes 2-D input as per-row 1-D FFTs; C does a full 2-D FFT — FIXED (7943d427)
Severity: High — fix
Rust: `crates/ad-plugins-rs/src/ioc.rs:196` hardcodes `Rows1D`; `fft.rs:382-439` → dims `nFreqX×height`; the `Full2D` path is never selected.
C: `NDPluginFFT.cpp:298-315,369-370` selects rank from ndims → `computeFFT_2D` → `nFreqX×nFreqY`.
Impact: every 2-D input yields different dims AND magnitudes.
FIXED: removed the `FFTMode` enum (the dual source of truth for rank — config mode vs the input's actual ndims) and derive the rank from `src.dims.len()` every frame, the single authority C uses. `compute_fft` dispatches `(rank, direction)`; the existing 2-D forward path (already C-equivalent: separable row-then-column = full 2-D FFT, `nFreqX×nFreqY`, normalised by `w*h`) is now reachable. `process_array` gates the whole frame on rank∈{1,2} up front, so a 3-D+ array yields no NDArray and no first-row waveforms (C's `default: error; return`). `FFTProcessor::new()` no longer takes a mode (crate-local API change; only ioc.rs constructs it). Tests: test_adp9_processor_selects_2d_fft_from_input_rank, test_adp9_processor_keeps_1d_input_1d, test_adp9_processor_rejects_rank_above_2.

#### ADP-10: Overlay shape ordinals Text/Ellipse swapped vs the C enum — FIXED 7f0d95c4
Severity: High — fix
Fix: forward + inverse shape↔ordinal maps and the slot comment corrected to C `NDOverlayShape_t` (Cross=0, Rectangle=1, Text=2, Ellipse=3).
Rust: `crates/ad-plugins-rs/src/overlay.rs:466-499` maps `2=Ellipse, 3=Text`.
C: `NDPluginOverlay.h:9-13` `Cross=0,Rectangle=1,Text=2,Ellipse=3`.
Impact: `OVERLAY_SHAPE=2/3` draws the wrong shape vs C.

#### ADP-11: Codec COMPRESSOR ordinal mapping diverges (extra zlib/lz4hdf5 shift) — FIXED 39e07250
Severity: High — fix
Fix: ordinals 0-4 aligned to C `NDCodecCompressor_t` (NONE/JPEG/BLOSC/LZ4/BSLZ4); Rust-only zlib/lz4hdf5 moved to 5/6 so they never shadow a C ordinal. (Structural ADP-26 sign-off — keep vs remove the extra codecs — still open under #58.)
Rust: `crates/ad-plugins-rs/src/codec.rs:1080-1088` maps `1=JPEG,2=Zlib,3=Blosc,4=LZ4,5=LZ4HDF5,6=BSLZ4` (the comment `:1078` mis-states the C ordinals).
C: `Codec.h:12-18` `NONE=0,JPEG=1,BLOSC=2,LZ4=3,BSLZ4=4`.
Impact: `COMPRESSOR=2` → Blosc in C, Zlib in Rust; `=3` → LZ4 vs Blosc; `=4` → BSLZ4 vs LZ4. Different codec + bytes. (Structural cause: ADC-10.)

#### ADP-12: JPEG RGB2/RGB3 written with wrong dims and as grayscale — FIXED (487d3bd4)
Severity: Medium — fix
Rust: `crates/ad-plugins-rs/src/file_jpeg.rs:81-105` converts to RGB1 but leaves the stale `ColorMode=RGB2/RGB3` attribute; `ndarray.rs:407-419` then mis-reads dims.
C: `NDFileJPEG.cpp:67-78,158-167` width=dims[0], JCS_RGB, re-interleaves.
Impact: RGB2 `[x=5,c=3,y=4]` → JPEG SOF width=3,height=4,1 grayscale component. Every RGB2/RGB3 JPEG wrong.
FIXED at the root: `convert_rgb_layout` (ad-core-rs/color.rs) now tags its output with `dst_mode`'s ColorMode attribute instead of cloning the source's, so dims and attribute agree by construction. The JPEG writer (the only consumer that calls `.info()` on the *converted* array) then reads RGB1 → writes a correct width=x, height=y, 3-component RGB JPEG. Defect-family audit of the 5 `convert_rgb_layout` callers: color_convert.rs (876,894) overwrites ColorMode on its final output (intermediates unaffected); file_tiff.rs:120 reads `rgb1.dims[1]/[2]` directly (distinct — not via `.info()`); file_magick.rs:143 calls `.info()` on the *original* array (distinct); only file_jpeg.rs:87 read the converted array's `.info()` → SAME defect, now fixed. The broader color.rs converter family (mono_to_rgb1, yuv*→rgb1, rgb1_to_mono…) also clones ColorMode but no current consumer `.info()`-mis-reads them (distinct, latent — not fixed). Tests: test_adp12_rgb2/rgb3_jpeg_written_as_rgb_not_grayscale.

#### ADP-13: Stats centroid/profiles computed for ndims>2; C rejects them
Severity: Medium — fix
Rust: `crates/ad-plugins-rs/src/stats.rs:970-978,992` gates only color_size/ndims>=2.
C: `NDPluginStats.cpp:205,338` return asynError for ndims>2.
Impact: 3-D mono array — Rust overwrites CENTROID/SIGMA/PROFILE/CURSOR; C leaves them stale.
— FIXED 6cfb6d1e: centroid/profile/cursor gates changed from `dims.len()>=2` to `==2`, so Rust no longer computes wrong-slice values for >2-D arrays. Reachability correction: a 3-D *mono* array gets `color_size=dims[2]` (ndarray.rs:436), so it was already skipped unless dims[2]==1; the truly-reachable cases are 4-D arrays (color_size=0) and `[x,y,1]`. Boundary note: C's ndims>2 emission is partly UB (centroid/cursor read the uninitialised stack-local `NDStats_t stats`, NDPluginStats.cpp:430 — no zero-init) and partly defined (profileX/Y are calloc-zeroed); perfect parity is impossible (centroid is genuine UB). Rust now posts deterministic 0 centroid/cursor and leaves the profile waveforms unposted (stale) rather than computing a misleading slice — the residual profile-stale-vs-C-zero and centroid-0-vs-C-garbage differences are UB-adjacent and intentionally not replicated. Test test_adp13_ndims_gt_2_skips_centroid_and_profiles.

#### ADP-14: Stats profile/cursor index not clamped to last valid line; Rust emits zeros
Severity: Medium — fix
Rust: `crates/ad-plugins-rs/src/stats.rs:845-878,1013-1020` out-of-range → all-zero profile, CURSOR_VAL=0.
C: `NDPluginStats.cpp:341-362` clamps `MAX(.,0)`/`MIN(.,size-1)` → edge row/col/pixel.
Impact: hot pixel at the far edge / cursor beyond image — Rust zeros, C edge pixels.
— FIXED b1b6f336: compute_profiles clamps centroid/cursor line indices to [0,size-1] (`(c+0.5).max(0).min(size-1)`, `cursor.min(size-1)`) and always extracts the edge row/col; the cursor-value read clamps to the last pixel too. Reachable mainly via the user-set CursorX/CursorY params (centroid stays in-range for non-negative data). Test test_adp14_out_of_range_cursor_clamps_to_edge_not_zeros.

#### ADP-15: Stats histogram upper-boundary value clamped into the last bin; C counts it as above
Severity: Medium — fix
Rust: `crates/ad-plugins-rs/src/stats.rs:733-735,758-760` `.min(hs-1)` clamps a top-edge value into the last bin.
C: `NDPluginStats.cpp:42-54` `if (bin>histSize-1 || value>histMax) histAbove++`.
Impact: last-bin count, HIST_ABOVE, HIST_ENTROPY diverge.
— NOT APPLICABLE (re-verified, no code change): the Rust `.min(hs-1)` clamp provably never fires for an in-range value, and both the serial (stats.rs:753-760) and parallel (728-735) paths already route out-of-range values via `val < hmin → below` / `val > hmax → above`, identical to C's `value<histMin` / `value>histMax`. Proof the clamp is dead: in the `else` branch hmin≤val≤hmax, and the bin expression `(val-hmin)·(hs-1)/(hmax-hmin)+0.5` is maximised at val=hmax giving exactly `(hs-1)+0.5 = hs-0.5`, which `as usize` truncates to `hs-1`. So the pre-clamp index is ≤ hs-1 for every in-range value (the 0.5 margin absorbs any fp error). C's extra `bin>histSize-1` disjunct is likewise redundant given its scale = (histSize-1)/(histMax-histMin): bin>histSize-1 ⟹ value>histMax. Both implementations: `(int)/(as usize)` truncate non-negative operands identically. histogram[], HIST_BELOW, HIST_ABOVE and HIST_ENTROPY (deterministic from histogram) all match. The clamp is harmless defensive code; removing it is out of scope (risks a panic if the fp proof has a hole, no observable benefit).

#### ADP-16: ROIStat out-of-range/zero ROI returns zeros; C clamps to one edge pixel
Severity: Medium — fix
Rust: `crates/ad-plugins-rs/src/roi_stat.rs:218-222` offset>=size/zero → all-zero.
C: `NDPluginROIStat.cpp:241-260` clamps to ≥1 pixel and writes back clamped geometry.
Impact: per-ROI values and geometry readbacks diverge.
— FIXED 017d3497: new `clamp_roi_geometry` mirrors the C clamp loop (offset→[0,dim-1], size→[1,dim-offset]) so a degenerate ROI collapses to a single edge pixel instead of vanishing, and `process_array` writes the clamped Dim0/1Min, Dim0/1Size and array Dim0/1MaxSize back to the per-ROI readback params (NDPluginROIStat.cpp:250-261). Tests test_adp16_clamp_out_of_range_offset_to_one_pixel / _zero_size / _geometry_writeback_uses_clamped_values; test_empty_roi and test_roi_out_of_bounds updated from zero stats to the clamped edge pixel. (Built on the rank-dispatch restructure landed for ADP-17.)

#### ADP-17: ROIStat 1-D background includes nonexistent Y-edges
Severity: Medium — fix
Rust: `crates/ad-plugins-rs/src/roi_stat.rs:288-309` always treats ROI as 2-D (4 edges).
C: `NDPluginROIStat.cpp:57-79` — ndims==1 background is only the 2 X-end strips.
Impact: 1-D ROI NET diverges.
— FIXED a8ac8287: `compute_roi_stats` now reads the raw array dims and dispatches by rank like the C `doComputeStatistics` (NDPluginROIStat.cpp:30-139): 1-D sums one X strip for stats and the two X-end strips of width MIN(bgdWidth,sizeX) for background; 2-D keeps the four-edge border, summed exactly as C (including the degenerate thick-border double-count). The previous path derived geometry from `info()` (y_size==0 for a 1-D array → every stat zeroed) and used a 2-D distance-from-edge ring on all ranks. Test test_adp17_1d_background_uses_x_strips_only.

#### ADP-18: ROI 3-D RGB path ignores the requested output dataType
Severity: Medium — fix
Rust: `crates/ad-plugins-rs/src/roi.rs:266` builds output with `src.data.data_type()`, ignoring `config.data_type` (the 2-D path `:414` applies it).
C: `NDPluginROI.cpp:144,166-174` converts to the requested type for RGB and mono.
Impact: 3-D RGB ROI with ROI_DATA_TYPE set — wrong output type/byte width.
— FIXED 3064f514: extract_roi_3d now resolves the target type as `config.data_type.unwrap_or(src type)` and converts the source-typed buffer via `convert_data_type` (dropping the frame on a conversion error), the same shape as the 2-D path. Test test_adp18_3d_rgb_honors_output_data_type. Note: like the 2-D path, the scaled value is cast to the source type before the type conversion, so a fractional scale on an integer→float ROI truncates in both paths — a shared latent precision gap vs C's f64→target single conversion, not introduced here and out of scope for this finding.

#### ADP-19: ROI single-color selection not collapsed; ColorMode not forced Mono
Severity: Medium — fix
Rust: `crates/ad-plugins-rs/src/roi.rs:138-273` ignores collapse_dims, keeps size-1 color dim with RGB ColorMode.
C: `NDPluginROI.cpp:180-215` forces collapseDims, ColorMode=Mono, removes size-1 dims.
Impact: dim count, ColorMode readback, shape diverge.
— FIXED c87ac8c3: extract_roi_3d collapses size-1 dimensions when the user collapseDims param is set, and force-collapses + tags ColorMode=Mono when an RGB input's color axis selects down to 1 (single_color). With out_c==1 the extracted buffer is already [x,y] row-major for every RGB layout, so dropping the size-1 axes is data-preserving. Test test_adp19_single_color_collapses_to_2d_mono.

#### ADP-20: Overlay Cross collapses independent SizeX/SizeY into one square — FIXED 314b5912
Severity: Medium — fix
Rust: `crates/ad-plugins-rs/src/overlay.rs:188-217,467-471` uses `max(size_x,size_y)` for both arms.
C: `NDPluginOverlay.cpp:95-116` independent arms.
Impact: SizeX≠SizeY draws different pixels.

#### ADP-21: Overlay Rectangle one pixel too narrow/short (exclusive vs inclusive) — FIXED ad15297a
Severity: Medium — fix
Rust: `crates/ad-plugins-rs/src/overlay.rs:219-252` spans `x..x+width` exclusive.
C: `NDPluginOverlay.cpp:120-144` `ix<=xmax` inclusive (Size+1 wide).
Impact: border one px shorter each dim, right/bottom edges inboard.

#### ADP-22: Overlay extended chars (≥128) render in Rust; C skips them (signed char) — FIXED 5d767953
Severity: Medium — fix
Rust: `crates/ad-plugins-rs/src/overlay.rs:82-95,309-329` renders codes 160..255.
C: `NDPluginOverlay.cpp:210-211` signed `char` makes 128..255 negative → `<32` → skipped.
Impact: non-ASCII DisplayText draws pixels in Rust, nothing in C.

#### ADP-23: Transform color layout from array attribute vs C `NDColorMode` param
Severity: Medium — verify
Rust: `crates/ad-plugins-rs/src/transform.rs:133,152,185` derives layout from the array's ColorMode attr.
C: `NDPluginTransform.cpp:527-529` reads the operator-set NDColorMode param.
Impact: when attr and record disagree, channel handling diverges. Verify reachability of the mismatch.
— NOT APPLICABLE (verified, no code change): the mismatch is unreachable. C's `transformImage` reads the NDColorMode *param* (NDPluginTransform.cpp:528), but `NDPluginTransform::processCallbacks` calls `NDPluginDriver::beginProcessCallbacks` first (line 485), which overwrites that param from the input array's `ColorMode` attribute every frame (NDPluginDriver.cpp:201-211, default Mono when absent) *before* transformImage runs. So C's param is just a per-frame copy of the array attribute at the point of use — an operator caput to NDColorMode is clobbered before transformImage reads it. The Rust `apply_transform` derives `info.color_mode` from the same `ColorMode` attribute via `info()` (same Mono default), so both pick layout from identical data. Equivalence already covered by test_rgb1_flip_horiz_keeps_color_grouping / test_rgb1_rot90cw_swaps_dims_and_keeps_color (RGB1 array carrying a ColorMode attribute). (The C `NDArraySizeZ=3` hardcode at line 513 is a separate readback-param quirk, not this finding.)

#### ADP-24: Process flat-field substitutes the field mean when scaleFlatField ≤ 0
Severity: Medium — fix
Rust: `crates/ad-plugins-rs/src/process.rs:369-373` uses the flat-field mean when scaleFlatField≤0.
C: `NDPluginProcess.cpp:172` `value *= scaleFlatField/flatField[i]` unconditionally (≤0 → 0).
Impact: SCALE_FLAT_FIELD≤0 — C all-zero, Rust mean-normalized.
— FIXED bbd7fd5e: flat-field uses self.config.scale_flat_field directly; mean fallback removed; tests test_flat_field (rewritten to C-direct) + test_adp24_scale_flat_field_zero_zeroes_output.

#### ADP-25: FFT FFTTimeSeries/FFTTimeAxis posted at unpadded width; C posts the padded length — FIXED (61b9964d)
Severity: Medium — fix
Rust: `crates/ad-plugins-rs/src/fft.rs:333-335,674,686` posts at `width`.
C: `NDPluginFFT.cpp:224,245-253` posts at `nTimeX` (next-pow-2).
Impact: non-pow-2 width — waveform length/content diverge.
FIXED: `compute_row_spectrum` builds the time series at the padded length `nTimeX = next_pow2(width)`, zero-extending past the input (C's `timeSeries` is the nTimeX calloc buffer with input copied into [0,width)). `n_time` then drives FFTTimeAxis at the padded length. FFTReal/FFTImaginary/FFTAbsValue/FFTFreqAxis already used `nFreqX = padded/2` and are unchanged. Test: test_adp25_timeseries_and_timeaxis_use_padded_length (width=5 → length-8 zero-extended TimeSeries/TimeAxis, length-4 Real/FreqAxis).

#### ADP-26: Codec implements zlib/lz4hdf5 codecs absent from the C reference
Severity: Medium — signoff
Rust: `crates/ad-plugins-rs/src/codec.rs:229-289,318-420`; names from `ad-core/codec.rs:9-27`.
C: `Codec.h:4-18` codec universe is `{"","jpeg","blosc","lz4","bslz4"}`.
Impact: a Rust array tagged "zlib"/"lz4hdf5" cannot be decompressed by stock C NDPluginCodec. Structural cause of ADP-11. Sign-off vs the ordinal-only fix.
RESOLVED 2026-06-15 — keep the extra zlib/lz4hdf5 codecs (user): additive; the ordinal-shadowing they caused is already fixed (ADP-11 @ 39e07250, moved to 5/6 so they never collide with a C ordinal). Files tagged with them are Rust-to-Rust by design.

#### ADP-27: netCDF missing NDNetCDFFileVersion global; writes extra uniqueId/numArrays globals — FIXED dbe09fd4
Severity: Medium — fix
Rust: `crates/ad-plugins-rs/src/file_netcdf.rs:469-495` writes uniqueId/numArrays globals, no NDNetCDFFileVersion.
C: `NDFileNetCDF.cpp:96-101` writes NDNetCDFFileVersion=3.1; uniqueId/numArrays are a variable/dimension, not globals.
Impact: a version-gating reader fails; Rust files carry two extra globals.
Fix: added the `NDNetCDFFileVersion` NC_DOUBLE=3.1 global (C :99, NDFileNetCDF.h:19) and removed the extra `uniqueId` (a per-frame variable, C :183) and `numArrays` (the unlimited dimension, C :119) globals. Regression `test_global_attrs_match_c_set` asserts the version double is present and the two extras are absent while uniqueId remains a variable.

#### ADP-28: TIFF RGB2/RGB3 written chunky (no PlanarConfig); C writes PlanarConfig=2 separate planes — SIGNOFF (routed to #58)
Severity: Medium — fix→signoff
Rust: `crates/ad-plugins-rs/src/file_tiff.rs:115-135,243-313` converts to interleaved RGB1, never writes PlanarConfiguration.
C: `NDFileTIFF.cpp:204-219,390-405` PLANARCONFIG_SEPARATE + planar strips.
Impact: a reader branching on PlanarConfiguration sees 2 (C) vs 1/absent (Rust).

Analysis (decoded-equivalent vs byte-faithful fork):
- C color-mode → tag map (NDFileTIFF.cpp:180-219): ndims 1/2 → Mono, CONTIG; ndims 3 + dims[0]==3 + RGB1 → samplesPerPixel=3, **PLANARCONFIG_CONTIG** (chunky); dims[1]==3 + RGB2 → rowsPerStrip=1, **PLANARCONFIG_SEPARATE**; dims[2]==3 + RGB3 → **PLANARCONFIG_SEPARATE**. writeFile (:385-405) writes RGB1 as one chunky strip; RGB2 as per-row red/green/blue strips into separate planes; RGB3 as three whole-plane strips.
- Rust (file_tiff.rs:115-135) normalizes **all** 3-D color to interleaved RGB1 via `convert_rgb_layout(...→RGB1)`, then writes through the `tiff` crate's high-level `ImageEncoder`, which is chunky-only and **omits** `Tag::PlanarConfiguration` (tiff-0.9.1 encoder/mod.rs:373-390 writes width/length/bits/sampleformat/photometric/rowsperstrip/samplesperpixel — no PlanarConfiguration). An absent tag defaults to 1=Chunky per the TIFF spec.
- Decoded result: identical. Both files are valid RGB TIFFs of the same dimensions and pixel values; a standards-compliant reader (libtiff/PIL/the `tiff` crate) yields the same image from CONTIG or SEPARATE. For RGB1 input the two writers agree exactly (both chunky; C's explicit PlanarConfiguration=1 == Rust's absent-defaults-to-1). The only divergence is RGB2/RGB3: C tags PlanarConfiguration=2 and lays the bytes out per-plane; Rust re-interleaves to chunky and omits the tag.
- Fix cost: the `tiff` 0.9.1 `ImageEncoder` has no separate-plane mode — its strip offset/bytecount bookkeeping assumes chunky sample interleave. Emitting a true PLANARCONFIG_SEPARATE file means dropping to `DirectoryEncoder` and hand-writing the IFD + per-plane StripOffsets/StripByteCounts arrays across all 10 data types (i8…f64), duplicating the crate's tested strip logic, for zero decoded-image difference.

Recommendation: keep the chunky-RGB1 normalization (decoded-equivalent; chunky RGB is the universally-supported TIFF layout). Surface for sign-off because the parity contract names "file-format bytes/tags": if byte-faithful PLANARCONFIG_SEPARATE output is required (e.g. round-tripping through a C areaDetector reader that inspects PlanarConfiguration), the hand-rolled planar writer above is the work. No source change this round; routed to #58.

#### ADP-29: Blosc codec params (level/shuffle/compressor) dropped from stored metadata; default clevel 3 vs 5 — FIXED e92d42be
Severity: Low — fix-low
Rust: `crates/ad-plugins-rs/src/codec.rs:830-837` writes 0/0/0; `:790-797` default clevel 3.
C: `NDPluginCodec.cpp:399-403,894` stores real params, default clevel 5.
Impact: NTNDArray codec metadata 0/0/0 + different compressed bytes/size.
Note: verified the codec level/shuffle/compressor fields are NOT serialized to the NTNDArray wire (`codec.parameters` carries only the original scalar type, `epics-pva-rs/src/nt/nd_array.rs`) nor read by the HDF5 writer (it uses its own `blosc_*` config). The observable divergence is the **default clevel 3→5** (changes compressed bytes + `compressedSize`); the stored-params 0→real change matches C `Codec_t` and removes a latent divergence if the fields are ever serialized.

#### ADP-30: Stats HIST_BELOW/HIST_ABOVE param type Float64 vs C Int32 — FIXED 064b8011; TIFF extra IFD tags / RowsPerStrip — SIGNOFF (#58)
Severity: Low — fix-low / signoff
Rust: `crates/ad-plugins-rs/src/stats.rs:1037-1038,1188-1189` Float64; `file_tiff.rs` (via the `tiff` crate) emits Compression/Predictor/Resolution tags + RowsPerStrip≠height.
C: `NDPluginStats.cpp:627-628,827-828` asynInt32; `NDFileTIFF.cpp:231-238` exactly 8 tags, RowsPerStrip=sizeY.
Impact: HIST value integer-equal but param type differs; TIFF IFD tag set differs (pixels identical).

Part 1 (HIST_BELOW/HIST_ABOVE type) — FIXED 064b8011: registered both as `ParamType::Int32` and emitted via `ParamUpdate::int32` so the RBV is DBR_LONG, matching C asynParamInt32 + setIntegerParam. Defect-family sweep over the complete C `asynParamInt32, &NDPluginStats*` set (12 params) confirmed these two were the only sites still registered Float64; ProfileSizeX/Y and the control params were already Int32. Regression `test_adp30_hist_below_above_emitted_as_int32`.

Part 2 (TIFF extra IFD tags / RowsPerStrip) — SIGNOFF, routed to #58: the `tiff` crate's high-level `ImageEncoder` writes Compression/Predictor/X-YResolution/ResolutionUnit and a default RowsPerStrip — a wider IFD tag set than C's, with identical decoded pixels. Same decoded-equivalent, crate-controlled family as ADP-28 (TIFF planar config); suppressing the extra tags / forcing RowsPerStrip=height needs the same hand-rolled IFD writer. Surfaced for the user rather than silently changed.

Verified-equivalent (ad-plugins): bad_pixel formulas, overlay_font glyphs/bit order, transform index math (all 8×4), roi 2-D crop/bin/reverse/scale, fft butterfly/normalization/freq-axis/EMA, time_series wrap, TIFF/netCDF dataType maps, JPEG mono/RGB1, plain LZ4 framing.
Not audited (lower numeric density / large): attr_plot, attribute, circular_buff, gather, scatter, pos_plugin, std_arrays, passthrough, file_hdf5+hdf5_layout, file_nexus, file_magick, pva — warrant a follow-up round (HDF5/Nexus especially).

### scaler-rs (SCAL)

#### SCAL-1: Idle process does not re-post all S1..Sn channels with DBE_LOG
Severity: Medium — fix
Rust: `crates/scaler-rs/src/records/scaler.rs:597-787` has no monitor/always-mark hook; relies on generic change-detection (only changed fields, VALUE|LOG).
C: `scalerRecord.c:758-774` — `monitor()` (every idle process) unconditionally re-posts every S1..Sn with DBE_LOG.
Impact: a DBE_LOG subscriber on `SCALER:Sn` gets an event every idle process in C, none in Rust. Same family as round-1 MOT-1.

#### SCAL-2: Value-change monitor posts carry an extra DBE_LOG bit; C posts CNT/Sn/T/VAL/PR1/TP/FREQ with DBE_VALUE only
Severity: Low — fix-low
Rust: `crates/epics-base-rs/src/server/record/record_instance.rs:1914` — every changed scaler field posts VALUE|LOG (no per-field mask override).
C: `scalerRecord.c` value-change posts use DBE_VALUE only (`:372,425,427,430,478,582,588`); DBE_LOG appears only in the Sn sweep.
Impact: a DBE_LOG-only subscriber on CNT/T/VAL/PR1/TP/FREQ gets a value-change event in Rust where C delivers none. Inverse of SCAL-1; same root (per-field mask contract unmodeled).

#### SCAL-3: `arm(0)` disarm does not clear counts; C clears unconditionally
Severity: Medium — fix
Rust: `crates/scaler-rs/src/device_support/scaler_soft.rs:113-128` zeroes counts only when `start==true`.
C: `drvScalerSoft.c:315-329` zeroes `counts[i]` and writes 0 to each input PV unconditionally (disarm too), before setting acquiring.
Impact: after a stop, Rust's next read keeps the final count; C drops S1..Sn to 0.

#### SCAL-4: `reset()` zeroes counts; C reset leaves them
Severity: Low — signoff
Rust: `crates/scaler-rs/src/device_support/scaler_soft.rs:70-79` zeroes counts.
C: `drvScalerSoft.c:303-313` clears acquiring/presets only.
Impact: transient (next read repopulates on both sides); documented deviation. Signoff.
RESOLVED 2026-06-15 — keep Rust (user): the count-zeroing is transient (next read repopulates on both sides); same class as the approved keep-Rust batch.

#### SCAL-5 (RATE): `special("RATE")` posts a different field than C
Severity: Low — signoff
Rust: `crates/scaler-rs/src/records/scaler.rs:844-846` clamps RATE → framework posts RATE.
C: `scalerRecord.c:690-693` clamps `rate` but `db_post_events(&tp,...)` posts TP (apparent copy-paste bug).
Impact: a clamped RATE write posts RATE in Rust, a spurious TP in C. Replicating the C bug is not advisable. Signoff.
RESOLVED 2026-06-15 — keep Rust (user): C posts TP on a RATE clamp (copy-paste bug, `db_post_events(&tp,...)`); Rust posts RATE correctly. Reproducing the C bug is not wanted.

Verified-equivalent (scaler): count→done sequence, CNT/US/SS transitions, preset reconciliation (NINT vs trunc), COUT/COUTP, VAL=T-on-completion, FwdLink gating, UDF via clears_udf; device-support read/done/preset-compare (`>=`, preset>0 gate, once-per-arm).

### std-rs (STD)

#### STD-1: epid writes the OUTL output link with feedback OFF (no FBON gate)
Severity: High — fix
Rust: `crates/std-rs/src/records/epid.rs:1402-1412` `multi_output_links` returns OUTL whenever `!compute_skipped`, no FBON condition.
C: `devEpidSoft.c:220-224` gates the OUTL `dbPutLink` on `fbon && outl.type!=CONSTANT`.
Impact: with FBON=Off, Rust keeps pushing OVAL to the actuator; C stops.

#### STD-2: epid writes OUTL on a sub-MDT cycle (dt < mdt)
Severity: Medium — fix
Rust: `crates/std-rs/src/device_support/epid_soft.rs:71-73` returns early without setting `compute_skipped`, so OUTL is still written.
C: `devEpidSoft.c:125` returns before the OUTL write.
Impact: too-fast cycle re-writes a stale OVAL in Rust; C doesn't.

#### STD-3: epid writes OUTL on a CONSTANT-INP cycle
Severity: Medium — fix
Rust: `crates/std-rs/src/device_support/epid_soft.rs:46-51` sets `inp_constant` but not `compute_skipped`; OUTL still written.
C: `devEpidSoft.c:110-112` returns before the OUTL write.
Impact: constant-INP epid still pushes OVAL in Rust. Structural fix for STD-1/2/3: gate OUTL on `fbon && !compute_skipped && !inp_constant && dt>=mdt` (or set `compute_skipped` on those paths).

#### STD-4: epid VAL deadband (MDEL/ADEL) monitor never fires — MLST/ALST double-advanced
Severity: High — fix
Rust: `crates/std-rs/src/records/epid.rs:418-435` `update_monitors` pre-advances mlst/alst to val; the framework's `check_deadband_ext` (`record_instance.rs:2153-2206`) then sees no delta and posts nothing.
C: `epidRecord.c:346-374` computes `delta=mlst-val`, posts VAL when `delta>mdel`, THEN sets `mlst=val`.
Impact: a `camonitor VAL` never receives an MDEL crossing event. Structural fix: `update_monitors` must not touch mlst/alst; `check_deadband_ext` is the single owner.

#### STD-5: timestamp `.%03f` fractional seconds truncate instead of round
Severity: Medium — fix-low
Rust: `crates/std-rs/src/records/timestamp.rs:165` `timestamp_subsec_millis()` truncates.
C: `epicsTime.cpp:235-239` adds `div/2` → rounds to nearest ms.
Impact: TST 9/10 non-device-time — last digit off by one at rounding boundaries (nsec=1_700_000 → C .002, Rust .001).

#### STD-6: timestamp posts a VAL monitor every process cycle even when the formatted string is unchanged
Severity: Medium — fix
Rust: `crates/std-rs/src/records/timestamp.rs:206-211` always replaces VAL; String field → `check_deadband_ext` forces a VALUE|LOG post every cycle.
C: `timestampRecord.c:152-163` posts VAL/RVAL only when the new string differs from OVAL.
Impact: a timestamp scanned faster than its format resolution gets redundant VAL updates in Rust.

#### STD-7: time_of_day VAL uses wall clock, not the record timestamp (TSE source) — FIXED c079c35e
Severity: Low — signoff
Rust: `crates/std-rs/src/device_support/time_of_day.rs:48,101` use `Local::now()`/`SystemTime::now()`.
C: `devTimeOfDay.c:121,145` use `recGblGetTimeStamp` (TSE/TSEL-selected).
Impact: default TSE=0 identical; diverge only for a non-current time source. Signoff.

FIXED c079c35e (user: Match C, same commit as STD-8) — both device supports now resolve the record's time stamp from TSE via a single owner `recgbl::get_time_stamp(tse, device_time)`, the `recGblGetTimeStamp` equivalent. `apply_timestamp` (database/mod.rs) was refactored to route through that same helper, so the value the support formats during `read()` and the stamp the framework applies one step later can never use two different TSE rules. `ProcessContext` now carries `dbCommon.time` (the device-time passthrough for TSE=-2). For TSE=0 the output is unchanged (current time); TSE!=0 now matches C instead of the wall clock.

#### STD-8: time_of_day omits the C `<undefined>` epoch-zero sentinel — FIXED c079c35e
Severity: Low — signoff
Rust: `crates/std-rs/src/device_support/time_of_day.rs:56-60` always formats a date.
C: `epicsTime.cpp:176-180` writes `"<undefined>"` for secPastEpoch==0 && nsec==0.
Impact: unreachable given the wall-clock source (STD-7). Signoff.

FIXED c079c35e (user: Match C, same commit as STD-7) — `createString` now emits the literal `"<undefined>"` when the TSE-resolved stamp's EPICS-epoch `secPastEpoch == 0 && nsec == 0` (`epicsTime.cpp:176`), matching `epicsTimeToStrftime`. The ai path (`aiReadTs`) needs no separate sentinel: an epoch stamp yields `secPastEpoch == 0`, so `val == 0.0`. Tests cover both the Unix and EPICS (1990) epochs and that a fixed device time formats its own year, not the wall-clock year.

Verified-equivalent (std): epid PID arithmetic term-by-term (error/P/I windup+DRVL/DRVH/ki==0/derivative/MaxMin/ODEL/bumpless seeding), throttle record (limit gate, clip, DRVLS, CONSTANT-OUT/SENT/FLNK, delay timer, WAIT, last-value-wins), time_of_day format strings, SecPastEpoch ai.

### optics-rs (OPT)

#### OPT-1: orient `Mode` maps constraints 1 and 2 swapped vs C
Severity: High — fix
Rust: `crates/optics-rs/src/snl/orient.rs:964-967` `1=>PhiConst, 2=>MinChiPhiMinus90`.
C: `orient.h:27-29` `MIN_CHI_PHIm90=1, PHI_CONST=2`; `orient.db:200-204` mbbo confirms.
Impact: Mode=1/2 run the wrong constraint → published TTH/TH/CHI/PHI motor setpoints differ.

#### OPT-2: orient singular A0/OMTX calc publishes a stale matrix; C publishes identity
Severity: Medium — fix
Rust: `crates/optics-rs/src/snl/orient.rs:356-359,364+` leave `a0`/`omtx` stale on a singular result; `:468,471` publish them.
C: `orient.c:188-201,279-285` fill identity on singular; `orient_st.st:497-503` pvPut identity + state=FAILED.
Impact: A0_11..A0_33 / OMTX_11..OMTX_33 read identity in C vs stale in Rust.

#### OPT-3: orient invertArray `x/det` vs `x*(1/det)`
Severity: Low — signoff
Rust: `crates/optics-rs/src/math/matrix3.rs:68-85` multiplies by `1/det`.
C: `matrix3.c:106-110` divides each element by det.
Impact: last-ULP difference on published A0_*/OMTX_* elements. Signoff (de-precisioning to match C).

#### OPT-4: kohzu/ml-mono soft-limit rejection leaves the rejected setpoints on the PVs (no prev-value revert)
Severity: High — fix
Rust: `crates/optics-rs/src/snl/kohzu_ctl.rs:905-916` (+`kohzu_ctl_soft.rs:531-535`, `ml_mono_ctl.rs`) only writes a message; the out-of-range setpoints already written stay.
C: `kohzuCtl.st:997-1013` reverts and pvPuts all six PVs to `prev_*`.
Impact: rejected out-of-range energy/lambda/theta/Y/Z setpoints persist in Rust vs restored prior in C.

#### OPT-5: kohzu/ml-mono energy/lambda/theta tweak (inc/dec) feature entirely unimplemented
Severity: High — fix
Rust: `crates/optics-rs/src/snl/kohzu_ctl.rs`, `ml_mono_ctl.rs` — tweak BOs/step PVs never created or monitored.
C: `kohzuCtl.st:734-797`, `ml_monoCtl.st:710-773` — inc/dec step ± tweakVal, limit-check, reset the BO.
Impact: tweak buttons do nothing, never clear, fire no alert. (kohzu_soft has no tweak state in C — correctly absent.)

#### OPT-6: kohzu/ml-mono forbidden-reflection / invalid-order Alert flag PV never written
Severity: Medium — fix
Rust: `crates/optics-rs/src/snl/kohzu_ctl.rs:687-742` (+`kohzu_ctl_soft.rs:398`) discard the `forbidden` flag; `ml_mono_ctl.rs:514-545` sets Alert only on Order<1, never clears.
C: `kohzuCtl.st:806,817`, `kohzuCtl_soft.st:700,711`, `ml_monoCtl.st:782,792` set/clear opAlert.
Impact: forbidden (H,K,L) leaves Alert 0 in Rust (C sets 1); not cleared on return to valid.

#### OPT-7: ml-mono standalone Y-motor move does not retrack yOffset → Z setpoint
Severity: Medium — fix
Rust: `crates/optics-rs/src/snl/ml_mono_ctl.rs:358-374` monitors the soft yOffset PV, not `{M_Y}.RBV`.
C: `ml_monoCtl.st:1204-1207,689,885` — a Y RBV change updates yOffset → Manual → recompute zMotDesired.
Impact: a standalone Y move leaves Z geometry stale in Rust.

#### OPT-8: PF4 Al/Ti/Glass transmission uses the Chantler table instead of the SNL's analytic absorption fits
Severity: High — fix
Rust: `crates/optics-rs/src/snl/pf4.rs:101-119` routes Al/Ti/Glass through `find_material` + linear interp; `:23,77-81` substitutes Si for borosilicate glass.
C: `pf4.st:484-505` (Al 7-term poly + 60 keV cap), `:509-538` (Ti K/L-edge branches), `:553-610` (Glass 8-oxide multi-edge); only "Other" uses the table.
Impact: `{H}trans{B}`/`{H}invTrans{B}` + 16 `fPos{B}.*ST` labels differ for every non-"Other" blade; glass is a different element/density entirely.

#### OPT-9: PF4 filterAl/filterTi/filterGlass written unconditionally (incl. 0.0) where C writes only when present
Severity: Low — fix-low
Rust: `crates/optics-rs/src/snl/pf4.rs:355-387,698-706` always sets/posts them.
C: `pf4.st:277-279` pvPuts each only when a blade uses that material.
Impact: `{H}filterAl|Ti|Glass` overwritten with 0.0 in Rust vs stale prior in C.

#### OPT-10: flexCombinedMotion issues an extra fine-motor pvPut on max-retries give-up
Severity: Medium — fix
Rust: `crates/optics-rs/src/snl/flex_combined_motion.rs:380-386` at give-up sets `move_fine` → `{FM}.VAL` write.
C: `flexCombinedMotion.st:272-276` transitions to `resetBusy`, no `{FM}.VAL` write on give-up.
Impact: an extra observable `{FM}.VAL` setpoint write in Rust.

#### OPT-11: QXBPM set_defaults wipes the calibrated dark-current offsets C preserves
Severity: Medium — fix
Rust: `crates/optics-rs/src/snl/qxbpm.rs:571` `set_defaults` resets calibration incl. zeroing `offset[]`.
C: `sncqxbpm.st:559-583` set_defaults never touches `offset[]`.
Impact: after a calibration, every `current:a..d` diverges by `trim*offset`.

#### OPT-12: QXBPM zero-quadrant-sum publishes 0.0 where C publishes NaN/±Inf — FIXED 3eeda648
Severity: Low — verify→fix
Rust: `crates/optics-rs/src/snl/qxbpm.rs:393-403` guards the sum and publishes 0.0.
C: `sncqxbpm.st:493-494` unguarded → ±Inf/NaN to `pos:x`/`pos:y`.
Impact: no-beam → C NaN, Rust 0.0. Verify whether the guard is intentional.
Resolved: matched C (NaN/±Inf is the intentional no-beam sentinel a client reads on `pos:x`/`pos:y`; the 0.0 guard masked it as a real origin position). Removed the guard so the unguarded divide publishes the same IEEE result as C.

#### OPT-13: Io startup init writes only `E_using`; C force-writes 19 default PVs
Severity: Medium — verify
Rust: `crates/optics-rs/src/snl/io.rs:578-585` writes only E_using.
C: `Io.st:174-196` pvPuts 19 constants + seeds 4 outputs.
Impact: Rust keeps stale DB/autosave values, outputs unseeded. (C's clobber-DB is arguably the questionable side — verify which to match.)
Investigated (2026-06-15): the 19 PVs C force-writes (`Io.st:174-195`) are all operator-tunable defaults, not fixed physical constants — the C comments say so (`icChannel` "likely scaler channel", `VperA` "likely setting", `xAir=1` "assume 1 atmosphere", `activeLen=60` "assume CHESS ion chambers", `dEff=1` "assume NaI(Tl)"). They are exactly the fields autosave restores (gas mix, chamber geometry, gains), so C's boot-time force-write clobbers autosaved operator settings. This is a genuine semantic fork, NOT output-form parity: (A) write all 22 like C (clobbers autosave for the 18 tunables); (B) seed only the 4 computed outputs (flux=0/ionPhotons=0/ionAbs=1/detector=0), leave operator params to autosave/.db defaults; (C) keep current (E_using only). Needs user sign-off; not silently picked.
Resolved 2026-06-15 (user chose option B), Fixed 7e968506: run() init now seeds flux=0/ionPhotons=0/ionAbs=1/detector=0 (Io.st:192-195); the 18 operator-tunable defaults (Io.st:174-191) are deliberately left to autosave/.db so a saved configuration is not clobbered on boot.

#### OPT-14: Io `scaler.DESC` (icName) string never written by Rust
Severity: Medium — fix
Rust: `crates/optics-rs/src/snl/io.rs:499-666` never writes `{P}scaler.DESC`.
C: `Io.st:351-364` pvGets `{VSC}.NMn` and PVPUTSTRs it into `{P}scaler.DESC`.
Impact: C keeps scaler.DESC updated; Rust never writes it.
Fixed 18a2795b: run() connects {VSC}.NM2..NM15 and writes the selected channel's name to {P}scaler.DESC each update (gated on channel 2-15).

#### OPT-15: Io coefficient/edge-case divergences (clamp + truncated constants)
Severity: Low — fix-low
Rust: `crates/optics-rs/src/snl/io.rs:648` clamps `icChannel.max(2)`; `:372-374` zeros outputs incl. ion_abs on ticks<=0; `:133-241,312-317` 5-sig-fig constants.
C: `Io.st:409-426,427,444,592-770` — channel 0/1 → cps=0; ticks==0 → finite ionAbs; 6-sig-fig constants.
Impact: (a) channel 0/1 → C zero outputs vs Rust nonzero; (b) ticks==0 → C-finite ionAbs vs Rust 0; (c) ~5th-sig-fig drift.
Fixed: (a)+(b) 67fa074b — eval_flux gates cps on the 2-15 channel window (0/1/>15 → cps=0, matching C switch default) and run() drops the .max(2) clamp; the ticks<=0 early-return is removed so ionAbs stays finite (cps→0 zeros flux/detector/ionPhotons). (c) 01d9fc5d — restored the exact 6-sig-fig C absorption coefficients (Io.st:592-770) in abs_h/he/be/c/n/o/ar, abs_ar_photo, and photon().

#### OPT-16: PF4 invTrans posted unconditionally where C gates on trans > 0 (found during OPT-12 family sweep)
Severity: Low — fix
Rust: `crates/optics-rs/src/snl/pf4.rs` recalculate/BitsChanged/FilterPosChanged each set `write_inv_transmission = Some(+inf)` when transmission is 0.
C: `pf4.st:281-282` `PVPUT(trans,...); if(trans>0.0) PVPUT(invtrans,1/trans);` — a zero transmission (glass blade < 2 keV) leaves `{H}invTrans{B}` at its prior value.
Impact: `{H}invTrans{B}` carries +inf in Rust vs stale prior in C for a fully-absorbing position. Sibling of OPT-9; closed structurally via a single `emit_transmission` owner. Fixed de2beb9f.

#### OPT-T1..T6 (table record): six candidates NOT independently verified
Severity: unknown — verify
The table sub-agent flagged: T-1 YANG offset rotation, T-2 speed-restore mask gate, T-3 limit-read-failure zeroing, T-4 Newport limit matrix frame, T-5 sqrt/asin clamps, T-6 speed-ratio NaN guard, in `crates/optics-rs/src/records/table.rs` vs `tableRecord.c`. Confirm at file:line on both sides before any fix.

Verified 2026-06-15 (all six confirmed REAL output-form divergences, each line-checked on both sides):
- **T-1 Fixed 7b443c58** — C special() (tableRecord.c:626-633) rotates ax0 by (old-new) yaw; Rust dropped it. Now tracks curr_yang and applies the two-pass RotY.
- **T-2 Fixed d7318d2d** — C RestoreMotorSpeeds (tableRecord.c:998-1006, called unconditionally) restores saved speed on every can_RW_speed motor; Rust gated on motor_move_mask. Mask gate dropped.
- **T-3 Fixed 9dc07384** — C GetMotorLimits (tableRecord.c:1024-1031) zeroes h0x/l0x on read failure; Rust kept the stale value. Pre-zero in pre_process_actions; reads overwrite on success.
- **T-4 Fixed c82703ec** — C NaiveMotorToPivotPointVector (tableRecord.c:1281) rebuilds the Newport matrix from raw ax for the translation-limit norm; Rust reused the offset+yaw matrix. Rebuild from raw ax for Newport.
- **T-5 OPEN (signoff)** — C does bare sqrt()/asin()/cos-division (tableRecord.c:1327,1333,1435-1438,...) producing NaN/±Inf at AY≈90° / asin round-off past ±1; Rust clamps to a finite value (Rust is the more-correct side; equivalent across the normal operating range, divergent only at the singular boundary). Matching C means reproducing its boundary NaN/Inf; keeping the clamp corrects a latent C round-off defect. Genuine fork — needs user sign-off (cf. OPT-12 matched C's NaN, but MODB-1/2/3 kept Rust's correction of a latent C defect).
- **T-6 OPEN (signoff)** — C computes speed_ratio = MIN(speed_ratio, sv0x[i]/v0x[i]) for every can_RW_speed motor with no guard (tableRecord.c:556-566); a stationary zero-saved-speed motor gives 0/0 = NaN, MIN(real,NaN)=NaN poisons speed_ratio so C skips the `<1` down-scaling and writes un-scaled speeds. Rust guards `v[i] > 0.0` and scales correctly. Same Rust-is-more-correct fork as T-5 — needs user sign-off.

Precision note (not a fixable finding): C `orient.c:15` `M_PI 3.14159265359` and kohzu/ml_mono `radConv 57.2958` are lower-precision than Rust's `std::f64::consts::PI`; Rust is *more* accurate. ~1e-6 relative on published angles. Surfaced, not "fixed" by de-precisioning.
Verified-equivalent (optics): matrix3 ops, orient per-constraint math, table forward/inverse geometry (4 geometries incl. 5-motor Newport), xia_slit/xiahsc/hsc raw↔dial, chantler table (22 species digit-for-digit), filter_drive/hr_ctl math, db_access re-export.

### modbus-rs (MODB) — no fix-required divergence

#### MODB-1: ASCII LRC — Rust implements the spec; C has a latent off-by-one that validates the wrong byte
Severity: Low — signoff
Rust: `crates/modbus-rs/src/interpose.rs:252-254` computes the spec-correct LRC over slave+data (NOTE `:246-251`).
C: `modbusInterpose.c:423-430` sums body+received-LRC (→0x00 for any valid frame) and compares against `data[i]` one byte past the decoded region (stale buffer) — never validates the real LRC.
Impact: on valid uppercase frames they agree; Rust rejects a genuinely-wrong LRC that C does not check. Intentional correction of a buggy C path. Signoff.
RESOLVED 2026-06-15 — keep Rust (user): matching C would reproduce a checksum path that never validates the real LRC. Rust's spec-correct LRC is the intended behavior.

#### MODB-2: ASCII reader rejects lowercase hex / runt frames that C silently mis-handles
Severity: Low — signoff
Rust: `crates/modbus-rs/src/interpose.rs:92-98,204-208,239-244` errors on non-`0-9A-F` and sub-minimum frames.
C: `modbusInterpose.c:218-222,400-406` decodes lowercase to garbage / returns empty-success on runts.
Impact: no wire divergence on valid frames; Rust stricter. Signoff.
RESOLVED 2026-06-15 — keep Rust (user): rejecting lowercase-hex/runt frames is the intended strictness; C's garbage/empty-success decode is a latent defect not worth reproducing.

#### MODB-3: Request frame-size overflow guarded in Rust, unchecked in C
Severity: Low — signoff
Rust: `crates/modbus-rs/src/interpose.rs:151,161,172,270-276` errors above MAX_MODBUS_FRAME_SIZE=600.
C: `modbusInterpose.c:260-263` memcpy into the fixed buffer with no bound check.
Impact: no divergence for valid-size requests; Rust adds a guard. Signoff.
RESOLVED 2026-06-15 — keep Rust (user): the frame-size bound prevents an unchecked overflow C has; matching C would reintroduce the unguarded memcpy.

Verified-equivalent (modbus): CRC-16 (0xA001/0xFFFF/low-byte-first), MBAP build+unwrap+txid correlation, all request PDUs (FC 1/2/3/4/5/6/15/16/23 incl. coil LSB-first packing, byteCount, FC23 read mode), response parse + exception 0x80 + code-5-as-success, all 37 data-type conversions (int16/uint16/int16sm/bcd/int32/uint32/int64/uint64/float32/float64 + LE/BE/BS variants, word order ABCD/CDAB/BADC/DCBA), BCD masking, string/zstring.

### mqtt-rs (MQTT)

#### MQTT-1: FLAT inbound payload whitespace-trimmed before parse; C parses raw and rejects surrounding whitespace
Severity: Medium — fix
Rust: `crates/mqtt-rs/src/payload.rs:182` `decode_flat` does `raw.trim()`.
C: `drvMqtt.cpp:248,282,286,376-385` parses raw; `isInteger` returns false on any non-digit char.
Impact: a FLAT:INT/FLOAT/DIGITAL payload `"42\n"` → VAL=42 in Rust, rejected (prior value kept) in C.

#### MQTT-2: FLAT:STRING inbound value trimmed; C stores the raw payload verbatim
Severity: Medium — fix
Rust: `crates/mqtt-rs/src/payload.rs:208` String uses the trimmed copy.
C: `drvMqtt.cpp:297-299` setStringParam(raw).
Impact: `"  hello  "` → Rust `"hello"`, C `"  hello  "`. (JSON:STRING path is correct.)

#### MQTT-3: JSON-format writes are encoded and published; C throws and publishes nothing
Severity: Medium — signoff
Rust: `crates/mqtt-rs/src/payload.rs:32-45`, `driver.rs:123-132` publish a JSON PUBLISH for `PayloadFormat::Json`.
C: `drvMqtt.cpp:587,629,656,692,722` all `throw "JSON support not implemented"` → asynError, no PUBLISH.
Impact: Rust adds outbound JSON the C lacks (the Z2M `/set` control records depend on it). Sign-off — closing it would delete the only outbound-JSON path Z2M uses.
RESOLVED 2026-06-15 — keep Rust JSON publish (user): functional superset; no C-produced output regresses (C only errored), and the Z2M `/set` path depends on it.

#### MQTT-4: `write_octet` publishes the whole string; C truncates the published payload at the first NUL
Severity: Low — fix-low
Rust: `crates/mqtt-rs/src/driver.rs:173-175` `from_utf8_lossy` publishes the full buffer.
C: `drvMqtt.cpp:714-716` → `publish(const std::string&)` from `stringData.data()` truncates at the first `\0`.
Impact: an embedded-NUL octet write publishes different bytes (Rust full, C up to NUL). Narrow.
- **Fixed 36f96ca1.** Defect family "asyn octet value is a NUL-terminated C-string" has two C-truncation sites, not just the cited publish: outbound `publish(stringData.data())` (`:716`) AND inbound store `setStringParam(index, val.c_str())` (`:299`) — the inbound site was not in the original citation but exhibits the same defect (Rust stored the full inbound payload past an embedded NUL). Closed both by construction with one `octet_cstr` helper (prefix up to the first NUL), applied at the outbound publish + cached value and the inbound octet store. C's inbound INT/FLOAT/DIGITAL paths parse the full `val` (no NUL truncation) → those stay raw (MQTT-1), distinct.

Verified-equivalent (mqtt): FLAT scalar float `%f` 6-decimal, FLAT float-array `%g`/6-sig (fixed/scientific + trailing-zero strip + C exponent), FLAT int/array/digital, masked-digital RMW merge, masked-write-on-undefined rejection, isBoolean INT/DIGITAL-only, JSON whole-key recursive search + explicit-null = not-found, QoS default 1 + retained=false, topic wildcard rejection.

### epics-tools-rs / procServ (PROC)

#### PROC-1: Telnet option negotiation blanket-refuses every request; C (libtelnet RFC1143) accepts ECHO and LINEMODE and stays silent on confirmations
Severity: High — fix
Rust: `crates/epics-tools-rs/src/procserv/telnet.rs:104-111` — any `DO opt` → `WONT opt`; any `WILL opt` → `DONT opt`; no per-option state, no ECHO/LINEMODE exception. The test `refuses_unknown_will` (`:189-203`) encodes the wrong behavior.
C: `libtelnet.c:453-461,396-403` accept ECHO (`WILL ECHO`) and LINEMODE (`DO LINEMODE`) per the option table (`clientFactory.cc:26-27`); confirmations of the server's own startup offers (`:475-478,417-420`) send nothing.
Impact: 4 wire-byte divergences vs any RFC client: `DO ECHO` → C `FF FB 01` vs Rust `FF FC 01`; `WILL LINEMODE` → C `FF FD 22` vs Rust `FF FE 22`; spurious refusal of the confirmation bytes. The Rust server contradicts its own `initial_negotiation()` (`telnet.rs:144-153`) → a standard client double-echoes or stops echoing.
- **Fixed 40770a28.** Ported libtelnet's RFC1143 Q-method (`_negotiate`) for ECHO/LINEMODE; the parser seeds each offered side to `WANTYES` at construction so confirmations stay silent and fresh ECHO/LINEMODE are accepted; unknown options stay refused. `QState` restricted to `{No, Yes, WantYes}` (the states reachable from a once-only, never-retracted offer); the `refuses_unknown_will` test was replaced with per-behaviour tests.

#### PROC-2: Info file and `PROCSERV_INFO` env use a `KEY=value` form `manage-procs` cannot parse; C writes `pid:`/`tcp:` lines and `PID=;CTL=` env
Severity: High — fix
Rust: `crates/epics-tools-rs/src/procserv/sidecar.rs:198-235` emits `procservpid=…\nchildpid=…\nchildexe=…\nchildargs=…` for both the info file and `PROCSERV_INFO` (comment `:9-11` claims it is "preserved exactly").
C: `procServ.cc:938-940,946-952` + `acceptFactory.cc:49,56-61` emit `pid:<pid>\n` + `tcp:<ip>:<port>\n` (info file) and `PID=<pid>;CTL=tcp:...;` (env); `manage.py:39-43` parses `pid:`/`tcp:`/`unix:`.
Impact: `manage-procs list/attach` finds neither PID nor listener address from a Rust info file; `PROCSERV_INFO` format differs. Both machine contracts break; the "preserved exactly" comment is false against this upstream.
- **Fixed 7bf565d8.** Info file now emits `pid:<supervisor-pid>` + `tcp:`/`unix:` lines; `PROCSERV_INFO` emits `PID=<supervisor-pid>;CTL=/LOG=tcp:…` with the trailing `;` stripped. `pid:` is `getpid()` (supervisor), the pid manage-procs probes for liveness — C never writes the child pid. `InfoSnapshot` carries listener addresses (from config) instead of child exe/args; `listen_addresses()` reproduces C's `connectionItem::head` order (prepend-reversed: log, unix, control).

#### PROC-3: Connection banner / child-lifecycle message strings rebranded `procserv-rs` and restructured
Severity: Medium — signoff
Rust: `crates/epics-tools-rs/src/procserv/supervisor.rs:561-617,431-438` — `@@@ Welcome to procserv-rs`, reworded Wrapping/kill/toggle lines, different child-exit text.
C: `clientFactory.cc:100,109-124` `@@@ Welcome to procServ (procServ-X.Y.Z)` + a single combined kill/restart-mode/toggle line; `procServ.cc:572-586,789-807` add server-PID/startup-dir/`Child "<name>" PID:`/shutdown lines and the `@@@ @@@ @@@ @@@ @@@` + sigChild exit banner.
Impact: nearly every `@@@` banner string differs verbatim; an operator script grepping `Welcome to procServ`/`Received a sigChild`/PID lines fails. Intentional branding rewrite — sign-off vs a string-by-string restoration.
RESOLVED 2026-06-15 — keep procserv-rs branding (user): the reworded banner / child-lifecycle strings stay; procserv-rs is a distinct product, not a drop-in console-string clone of procServ.

Verified-equivalent (procServ): IAC framing (0xFF, IAC-IAC unescape, outbound IAC doubling, `IAC SB..IAC SE` skip), initial offer bytes order, caret control-char rendering (c+64), RestartMode labels ON/OFF/ONESHOT, kill-command broadcast.

## Review Log

### Round 2 — 2026-06-15 (remaining 8 crates: ad-core, ad-plugins, scaler, std, optics, modbus, mqtt, procServ)

~70 output-form findings across 8 crates (read-only fan-out, 6 parallel agents).
Cluster summary:

- **areaDetector numeric/format fidelity (ADC, ADP).** The dominant cluster:
  pixel arithmetic (RGB→Mono weights ADP-2, false-color LUT ADP-3, Bayer
  border ADP-4), statistic formulas (centroid moments ADP-1, histogram edge
  ADP-15, integer-truncation in time-series/process ADP-8/ADP-24), enum/ordinal
  maps (overlay shape ADP-10, codec compressor ADP-11), processing order
  (clip ADP-5, auto-scale timing ADP-6, valid-flag invalidation ADP-7), and
  file-format structure (JPEG/TIFF/netCDF ADP-12/27/28). Output-form is the
  whole contract for an image pipeline.
- **Record monitor-post / output-link fidelity (SCAL, STD).** Same family as
  round-1 BASE-1/MOT-1: which monitor fires with what mask (SCAL-1/2, STD-6),
  and a structural epid OUTL-gating + deadband-ownership pair (STD-1..4) where
  the PID arithmetic itself is faithful.
- **SNL state-program output writes (OPT).** Constraint/mode maps (OPT-1),
  limit-reject revert (OPT-4), unimplemented tweak feature (OPT-5),
  alert-flag writes (OPT-6), and the PF4 analytic-vs-table transmission (OPT-8)
  — the divergences are in *which* PVs get written and the absorption physics,
  not the linear-algebra core.
- **Device-protocol bytes (MODB, MQTT, PROC).** modbus is clean (the only
  deltas are Rust correcting latent C defects). mqtt has an over-eager
  `raw.trim()` (MQTT-1/2). procServ has two genuine machine-contract breaks
  (telnet wire bytes PROC-1, info-file format PROC-2) plus a branding rewrite
  (PROC-3, signoff).

Disposition tally (pre-fix): ~46 **fix**, ~7 **fix-low**, ~10 **signoff**,
~9 **verify** (incl. the 6 table-record T-candidates and the unwired ad-core
library paths). Signoff items: ADC-7, ADC-10/ADP-26 (codec vocab), ADP-1
(stats centroid precision fork), ADP-28 (TIFF planar config), ADP-30b (TIFF
extra IFD tags), SCAL-4, SCAL-5, OPT-3, MQTT-3, PROC-3,
MODB-1/2/3 — surfaced for the user rather than silently changed. STD-7 and
STD-8 were re-dispositioned signoff→fix (user: "Match C") and ADC-8 verify→fix
(user: "fix-now"); all three are now Fixed (c079c35e, 515c1b5c).

### Round 3 — 2026-06-15 (ad-plugins-rs full-plugin sweep, ADP-31..95)

Second ad-plugins fan-out (4 parallel read-only agents) covering the plugin
modules round 2 did not reach: routing/buffer (`NDPosPlugin` /
`NDPluginCircularBuff` / `NDPluginScatter` / `NDPluginGather`), attribute/array
(`NDPluginStdArrays` / `NDPluginAttribute` / `NDPluginAttrPlot` / passthrough),
the `NDFileHDF5` writer, and the NeXus/Magick file writers + the PVA NTNDArray
converter. 65 findings, ADP-31..95.

Numbering note: Agent 1 (routing) and Agent 2 (attribute/array) both emitted an
ADP-46. Agent 1's ADP-46 was a parity-clean "no divergence" scatter
first-consumer-offset check — folded into ADP-45 (`NDPluginScatter`) below — so
ADP-46 is Agent 2's StdArrays finding and the sequence stays contiguous:
31-45 routing, 46-60 attr/array, 61-80 HDF5, 81-95 nexus/magick/pva.

Cluster summary:

- **Routing/buffer param surface (ADP-31..44).** `NDPosPlugin` is the worst —
  it registers none of its 17 asyn params and posts the two values it does
  compute to hardcoded port indices 0/1 (ADP-33), parses a position-XML format
  with entirely different attribute names than C (ADP-31), never builds the
  `CurrentPos` octet string (ADP-32), and forwards frames downstream while idle
  where C drops them (ADP-35). `NDPluginCircularBuff` carries the same family:
  `CIRC_BUFF_STATUS` is an asynOctet *string* in C but Int32 in Rust (ADP-40),
  and the TriggerA/B/CalcVal float params are never posted (ADP-41).
- **Attribute/array channel counts (ADP-46..60).** `NDPluginStdArrays` posts
  only the array's *native* element type, not all six asyn array interfaces C
  type-converts to (ADP-46); `NDPluginAttribute` hardcodes 8 channels and
  ignores the `maxAttributes` configure arg (ADP-47), which also fixes the TS
  waveform length (ADP-49). `NDPluginAttrPlot` is not wired into the IOC at all
  (ADP-52) — its other findings are latent until it is.
- **HDF5 NeXus default layout (ADP-61..80).** The largest single divergence:
  with no user XML (the default mode) C writes the image at
  `/entry/instrument/detector/data` inside a full NeXus group tree with
  `NX_class` group attributes and hardlinks; Rust writes a single flat `/data`
  dataset with none of it (ADP-61, ADP-62). Filter encodings diverge: N-bit
  is implemented as cd_values instead of datatype precision/offset + paramless
  `H5Pset_nbit` (ADP-67), SZIP uses the EC mask where C uses NN (ADP-66),
  string-attribute datasets are `[n,256]` u8 vs C's `[n]` fixed-length string
  (ADP-77).
- **PVA / NeXus / Magick output form (ADP-81..95).** Almost entirely
  parity-clean or deliberate-improvement signoff. The PVA NTNDArray wire
  structure is byte-faithful on every field except `dimension[].binning`, which
  clamps 0→1 where C serializes the raw value (ADP-86, the only wire divergence
  in this group). NeXus/Magick diverge from C only by *adding* correct output
  or *correcting* C bugs (RGB2/RGB3 uninitialized-pixel bug ADP-95), never by
  omitting/corrupting a C-written object.

Disposition tally (pre-fix): **22 fix, 9 fix-low, 9 verify, 25 signoff**.

- **fix (High/Medium):** ADP-31, 32, 33, 35, 36, 37, 38, 40, 41 (routing);
  46, 47, 48, 49 (attr); 61, 62, 63, 64, 66, 67, 71, 77, 79 (HDF5).
- **fix-low:** ADP-34, 43, 44 (routing); 50, 51 (attr); 65, 69, 72 (HDF5);
  86 (PVA).
- **verify:** ADP-42, 45 (routing); 52, 57, 59, 60 (attr); 68, 74, 80 (HDF5).
- **signoff:** ADP-39 (routing); 53, 54, 55, 56, 58 (attr); 70, 73, 75, 76, 78
  (HDF5); 81-85, 87-95 (PVA/NeXus/Magick).

Highest-impact fixes: ADP-33 (no param registration), ADP-31 (XML attr names),
ADP-40 (STATUS DBR type Octet vs Int32), ADP-41 (trigger value params never
posted), ADP-35 (idle frames forwarded), ADP-46 (single native-type post),
ADP-47 (maxAttributes ignored), ADP-61/62 (no default NeXus layout/NX_class),
ADP-67 (N-bit filter wrong).

## Round 3 Open Findings (ADP-31 – ADP-95)

### routing / buffer plugins — NDPosPlugin / NDPluginCircularBuff / NDPluginScatter / NDPluginGather (ADP-31..45)

#### ADP-31: NDPosPlugin XML position format is completely different (attribute names diverge)
Severity: High — fix — **FIXED 677e70a3**
Rust: `pos_plugin.rs:180-262` `parse_positions_xml` reads `<position index="N">value</position>`, emits a single map key literally named `"position"`.
C: `NDPosPluginFileReader.cpp:144-213` reads `<dimensions><dimension name="X"/>…</dimensions>` then `<position X="1" Y="2"/>`, where each map key is a **dimension name** and the value comes from a `<position>` **attribute** of that name.
Impact: Downstream NDArray attributes have entirely different names/structure. A real `pos_layout` XML (`<dimension name="x"/>` + `<position x="..."/>`) attaches attributes named `x`, `y`, … in C, but the Rust parser finds zero matches in that format and attaches nothing.

#### ADP-32: NDPos_CurrentPos octet param never produced
Severity: High — fix — **FIXED 6a788fe0**
Rust: `pos_plugin.rs:306-322` attaches attributes but posts only `ParamUpdate::int32(0,…)`/`int32(1,…)`; no CurrentPos string is built or posted.
C: `NDPosPlugin.cpp:149-166` builds `sspos = "[" + key "=" value ("," …) + "]"` and `setStringParam(NDPos_CurrentPos, …)`.
Impact: A client reading `NDPos_CurrentPos` (asynParamOctet) gets `"[x=1,y=2]"` in C; in Rust the param does not exist and is never updated.

#### ADP-33: NDPosPlugin registers none of its 17 params; processor emits hardcoded indices 0/1
Severity: High — fix — **FIXED 6d505b9f**
Rust: `pos_plugin.rs` has no `register_params`; `process_array` posts `ParamUpdate::int32(0, missing)` and `int32(1, duplicate)` to fixed indices 0/1.
C: `NDPosPlugin.cpp:383-399` `createParam` for 17 params with explicit asyn types (Octet: Filename, CurrentPos, IDName; Int32: FileValid, Clear, Running, Restart, Delete, Mode, Append, CurrentQty, CurrentIndex, MissingFrames, DuplicateFrames, ExpectedID, IDDifference, IDStart).
Impact: Clients cannot read CurrentQty/CurrentIndex/Running/ExpectedID/FileValid, and the two values that *are* posted land on whatever params live at indices 0/1 — not MissingFrames/DuplicateFrames — so even those go to the wrong DBR-typed params.

#### ADP-34: NDPosPlugin attribute description string differs ("" vs "Position of NDArray")
Severity: Low — fix-low — **FIXED 4423e6f6** (description fixed; the `source`/driverName-string sub-point is a port-wide `NDAttrSource::Driver`→"Driver" modeling decision, left as-is)
Rust: `pos_plugin.rs:308-313` `NDAttribute::new_static(key, String::new(), …)` — empty description.
C: `NDPosPlugin.cpp:161` `new NDAttribute(name, "Position of NDArray", NDAttrSourceDriver, driverName, NDAttrFloat64, &value)`.
Impact: The NDAttribute `description` (observable when attributes serialize to HDF5/PVA) is empty in Rust where C carries `"Position of NDArray"`; the `source` (driverName) string also differs (Rust passes empty).

#### ADP-35: NDPosPlugin forwards arrays downstream while idle; C drops them
Severity: High — fix — **FIXED fbe9db6c**
Rust: `pos_plugin.rs:266-268` when `!running`, returns `ProcessResult::arrays(vec![clone])` — forwards the input unchanged.
C: `NDPosPlugin.cpp:54,202-205` `endProcessCallbacks` is reached only inside `if (running == NDPOS_RUNNING)` and only `if (skip == 0 && running == NDPOS_RUNNING)`; when idle, no downstream callback.
Impact: Stopped (NDPOS_IDLE), C produces no downstream callbacks; Rust passes every frame through. Downstream plugins observe frames in Rust that C withholds.

#### ADP-36: NDPosPlugin expected-ID stepping ignores IDDifference and re-syncs to actual uniqueId
Severity: Medium — fix — **FIXED 9f53c170**
Rust: `pos_plugin.rs:317` `self.expected_id = array.unique_id + 1` (re-anchors to received ID, step hardcoded +1).
C: `NDPosPlugin.cpp:192-194` `expectedID += IDDifference` (steps by configurable `NDPos_IDDifference`, default 1, never re-anchoring).
Impact: With `IDDifference != 1`, or any sequence where uniqueId does not advance by exactly the step, the missing/duplicate classification and MissingFrames/DuplicateFrames values diverge. Even at step 1, Rust's re-anchor masks cumulative drift C keeps reporting.

#### ADP-37: NDPosPlugin first-frame ID check is suppressed (expected_id starts at 0)
Severity: Medium — fix — **FIXED 63d8951a**
Rust: `pos_plugin.rs:104,280` `start()` sets `expected_id = 0`; gate `if self.expected_id > 0` skips ID checking on the first running frame; expected is armed only after the first via `unique_id + 1`.
C: `NDPosPlugin.cpp:234,421-422` writeInt32(Running) sets `ExpectedID = IDStart` (default 1), so the first frame is compared against ExpectedID=1 immediately and can already be drop/duplicate.
Impact: A first frame whose uniqueId ≠ 1 is counted missing/duplicate in C but accepted silently in Rust, so the counters and which frame gets a position attached differ on the first frame after start.

#### ADP-38: NDPosPlugin position-exhaustion does not stop/abort the documented way
Severity: Medium — fix — **FIXED 3e1d2fa1**
Rust: `pos_plugin.rs:285-298` on a gap, advances `diff` times and, if positions run out, forwards the array **with no position attached** and returns.
C: `NDPosPlugin.cpp:98-124,140` on a gap in Discard mode erases from the front; if size hits 0 it sets `NDPos_Running = IDLE` (stops) and does **not** call endProcessCallbacks (no downstream emit). Keep mode advances index and stops at `index == size`.
Impact: Positions exhausted mid-gap: C stops the plugin and drops the frame; Rust forwards the bare frame and keeps running. Downstream gets an extra unattributed frame in Rust and Running stays on.

#### ADP-39: NDPosPlugin position values are Float64 in both (header comment says integer)
Severity: Low — signoff
Rust: `pos_plugin.rs:312` `NDAttrValue::Float64(*value)`.
C: `NDPosPlugin.cpp:161` `NDAttrFloat64` with a `double`.
Impact: None — both emit NDAttrFloat64, so the attribute type matches. The header comment (`NDPosPlugin.h:9`) says "1D integer valued attribute" but the C code attaches Float64; Rust matches the code. Listed for completeness.

#### ADP-96: PluginType param value mismatches C for NDPositionPlugin and NDAttrPlot
Severity: Low — fix — **FIXED 357dbba1** (found during the ADP-31..38 fix phase)
Rust: `pos_plugin.rs:551` `plugin_type()` returned `"NDPosPlugin"`; `attr_plot.rs:340` returned `"NDPluginAttrPlot"` (the class names). The runtime sets the `PLUGIN_TYPE` asyn param (PluginType_RBV) from `plugin_type()` (`runtime.rs:1103`).
C: `NDPosPlugin.cpp:402` `setStringParam(…PluginType, "NDPositionPlugin")`; `NDPluginAttrPlot.cpp:87` `setStringParam(…PluginType, "NDAttrPlot")`.
Impact: A client reading `PluginType_RBV` saw the wrong string for these two plugins. Defect-family sweep over all 24 `plugin_type()` impls: only these two diverged; the other 22 match C verbatim. `file_hdf5.rs` is distinct — C embeds the ADCore build version (`"NDFileHDF5 ver%d.%d.%d"`, `NDFileHDF5.cpp:2387-2388`), which the Rust port cannot fabricate, so it is intentionally left as `"NDFileHDF5"`.

#### ADP-40: NDPluginCircularBuff CIRC_BUFF_STATUS is an Octet string in C, Int32 in Rust
Severity: Medium — fix — **FIXED d26eec7f**
Rust: `circular_buff.rs:394-401` maps status enum to int32 `0..3`; `CIRC_BUFF_STATUS` registered as `ParamType::Int32`; no "Dropping frames"/"Buffer Wrapping" string states.
C: `NDPluginCircularBuff.h:12` `CIRC_BUFF_STATUS` is **asynOctet**, set to `"Idle"`, `"Buffer filling"`, `"Buffer Wrapping"`, `"Dropping frames"`, `"Flushing"`, `"Acquisition Completed"`, `"Acquisition Stopped"`, `"Stop acquisition to set pre-count"`, `"Pre-count too high"`, `"Invalid pre-count value"` (`NDPluginCircularBuff.cpp:153-260`).
Impact: The DBR type of `CIRC_BUFF_STATUS` differs (Octet vs Int32) and the status text a client reads is entirely different — a wire/param-type divergence, not just internal representation.

#### ADP-41: NDPluginCircularBuff TriggerAVal/TriggerBVal/TriggerCalcVal float params never posted
Severity: High — fix — **FIXED 3792f90c**
Rust: `circular_buff.rs:393-414` posts only status, current_image, triggered, actual_trigger_count; the Calc branch (`circular_buff.rs:199-224`) computes `a`, `b`, `expression.evaluate_vars` but never writes `trigger_a_val`, `trigger_b_val`, `trigger_calc_val`.
C: `NDPluginCircularBuff.cpp:67-78` `setDoubleParam(NDCircBuffTriggerAVal, args[0])`, `…TriggerBVal, args[1])`, `…TriggerCalcVal, calcResult)` on every frame's trigger calc.
Impact: Clients reading `CIRC_BUFF_TRIGGER_A_VAL`/`_B_VAL`/`_CALC_VAL` (asynFloat64) see live values in C; in Rust these registered Float64 params always remain at default 0.0.

#### ADP-42: NDPluginCircularBuff Calc trigger fires on NaN/Inf where C guards against it
Severity: Medium — verify
Rust: `circular_buff.rs:204-223` missing attribute → `f64::NAN` for A/B; trigger fires when `evaluate_vars(…) != 0.0`.
C: `NDPluginCircularBuff.cpp:43-77` args default to `epicsNAN`; trigger fires only when `!isnan(calcResult) && !isinf(calcResult) && (calcResult != 0)`.
Impact: When the calc result is NaN/Inf (e.g. expression `A` with A absent), C does **not** trigger; Rust's `!= 0.0` is true for NaN, so Rust fires a spurious trigger (pre-buffer flush + post frames) where C does not.

#### ADP-43: NDPluginCircularBuff currentImage not reset to 0 on stop
Severity: Low — fix-low — **FIXED 732f55bc**
Rust: `circular_buff.rs:489-491` on Control==0 sets `status = Idle` but does not zero the reported current-image count until the next reset.
C: `NDPluginCircularBuff.cpp:259` writeInt32(Control off) `setIntegerParam(NDCircBuffCurrentImage, 0)`.
Impact: After stopping, a client reading `CIRC_BUFF_CURRENT_IMAGE` reads 0 in C; in Rust the stop path leaves the last value posted until the next frame.

#### ADP-44: NDPluginCircularBuff pre-count validation status outputs not produced
Severity: Low — fix-low — **PARTIAL 9f8666c6** (running + negative rejects done; maxBuffers "Pre-count too high" deferred)
Rust: `circular_buff.rs:492-493` on pre_trigger change just clamps to `>=0` and stores; no rejection, no status feedback, no `maxBuffers-1` ceiling.
C: `NDPluginCircularBuff.cpp:280-292` rejects pre-count when running ("Stop acquisition to set pre-count"), when `> maxBuffers_-1` ("Pre-count too high"), when `<0` ("Invalid pre-count value"), and only then commits.
Impact: Setting an out-of-range/in-flight pre-count: C refuses the update and posts an explanatory `CIRC_BUFF_STATUS` string; Rust silently accepts the clamped value. The observable PRE_TRIGGER readback and STATUS differ.
DEFERRED: the "Pre-count too high" (> maxBuffers_-1) check is not implemented — the Rust CircularBuff processor carries no maxBuffers bound (the NDArrayPool buffer cap is not plumbed into it), and C's own check is degenerate at the common maxBuffers=-1 (unlimited) config. The running-reject and negative-reject (with status string + param revert) and the accept path are done.

#### ADP-45: NDPluginScatter overflow-reroute / nextClient wrap semantics not reproduced
Severity: Medium — verify
Rust: `scatter.rs:60-68` emits `current_index % num_outputs` (raw index when `num_outputs==0`), advancing by 1 each frame; the runtime maps it onto consumers. No replication of C's "skip a full-queue consumer, advance to next, drop only on the last node" logic.
C: `NDPluginScatter.cpp:59-90` walks the interrupt client list from `nextClient_`, sets `auxStatus = asynOverflow` for all but the last node so a full queue **advances to the next client** rather than dropping; only the final node drops; `nextClient_` persists/wraps (`if (nextClient_ > numNodes) nextClient_ = 1`).
Impact: Under backpressure C reroutes the frame to the next available consumer and keeps `nextClient_` advancing; Rust delivers strictly to `index % N` regardless of queue state, so per-consumer distribution and drop-vs-reroute differ. With all queues free the round-robin order matches. Verify because the routing is partly in the runtime, not audited here.
First-consumer offset (folded from Agent-1's ADP-46): parity-clean — C `nextClient_(1)` with 1-based `ellNth(1)` = first node, Rust `current_index` starts at 0 = first consumer; both send the first frame to the first registered consumer. Registration order is an IOC-wiring concern outside these files.
Module note — `gather.rs`: output-form parity-clean on the pass-through path (C `NDPluginGather.cpp:80-91` forwards each array; Rust `gather.rs:96-99` matches). The Rust-invented `GATHER_NDARRAY_PORT_N/ADDR_N/NUM_PORTS` params do not exist in C (C uses the base-class `NDPluginDriverArrayPort/Addr` multi-address params); an added param surface, not a C-output divergence — flagged for orchestrator, unnumbered.

### attribute / array plugins — NDPluginStdArrays / NDPluginAttribute / NDPluginAttrPlot / passthrough (ADP-46..60)

#### ADP-46: NDPluginStdArrays posts only the array's native element type, not all six asyn array types
Severity: High — fix — **FIXED 44dee647** (asyn-rs array I/O Intr now converts the carried native array to the consuming record's interface element type via `convert_param_array_to_iface`, mirroring the polled `result_to_value` path; residual unsigned-native→wider-signed-FTVL edge noted in the commit since the ParamValue carrier holds no unsigned array variant)
Rust: `plugin/runtime.rs:708-756` (`build_publish_batch`) fires a single `notify` whose `ParamValue` array variant is the NDArray's native `NDDataBuffer` type (F64→Float64Array, I32→Int32Array, …).
C: `NDPluginStdArrays.cpp:169-197` calls `arrayInterruptCallback` for **all six** interfaces (int8/16/32/64, float32/64), each running `pNDArrayPool->convert(pArray, &pOutput, signedType)` and pushing the type-converted copy to every subscribed client.
Impact: A waveform/aai record bound to STD_ARRAY_DATA whose FTVL differs from the array's native type (e.g. FTVL=SHORT fed by an F64 array) gets no correctly-converted update on the I/O Intr frame path. Confirmed: the interrupt consumer (`asyn-rs/adapter.rs:1249-1284`) takes the native-typed array verbatim and never routes through the typed `read_int16_array`/`read_int32_array` converters (those run only on the polled path, `runtime.rs:1393-1442`).

#### ADP-47: NDAttrConfigure ignores `maxAttributes` arg; channel count hardcoded to 8
Severity: High — fix — **FIXED c338aef3** (AttributeProcessor holds a Vec sized to maxAttributes; create_attribute_runtime threads num_channels=max(arg,1) + num_addr=max(arg,2); ioc reads arg 5 via the new C-faithful `attr_arg_defs`)
Rust: `attribute.rs:20` `MAX_ATTR_CHANNELS = 8`; `attribute.rs:228-236` always builds the runtime with 8; `ioc.rs:409`/`helpers.rs:65-79` `extract_plugin_args` never reads arg index 5 (`maxAttributes`).
C: `NDPluginAttribute.cpp:169-184` takes `maxAttributes`, sets `maxAttributes_` (floored ≥1), uses it as asyn address count `std::max<int>(maxAttributes,2)`; `processCallbacks` loops `i<maxAttributes_` (line 55).
Impact: The number of attribute channels (and ATTR_VAL/ATTR_VAL_SUM/ATTR_ATTRNAME records serviced, per-channel `callParamCallbacks(i)` posts) is fixed at 8 regardless of the `NDAttrConfigure` arg. A db configured for 16 loses channels 8-15; one for 2 still allocates 8. Also drives the TS length (ADP-49).

#### ADP-48: ATTR_RESET only clears on non-zero writes; C clears on every write
Severity: Medium — fix — **FIXED 0802be4c** (removed the `value != 0` guard; defect family also covered `attr_plot.rs` AP_Reset, fixed in the same commit since C `reset_data()` also runs on any write, NDPluginAttrPlot.cpp:290-292)
Rust: `attribute.rs:178-189` guards `if params.value.as_i32() != 0` before zeroing Val/ValSum.
C: `NDPluginAttribute.cpp:123-128` zeros `NDPluginAttributeVal`/`ValSum` for all channels on **any** write to `NDPluginAttributeReset` (no value test), then `callParamCallbacks()`.
Impact: `caput ATTR_RESET 0` in C zeros all Val/ValSum and posts monitors; in Rust it is a no-op. Downstream monitors observe a clear event in C that never fires in Rust for a zero write.

#### ADP-49: Attribute time-series array length is fixed at 8, not `maxAttributes_`
Severity: Medium — fix — **FIXED c338aef3** (subsumed by ADP-47: `attr_ts_channel_names(num_channels)` and the per-frame values Vec now track maxAttributes_)
Rust: `attribute.rs:146` collects exactly 8 channel values; `attr_ts_channel_names()` (197-208) defines 8 TS channels.
C: `NDPluginAttribute.cpp:93-110` `doTimeSeriesCallbacks` allocates an NDArray of `dims=maxAttributes_` NDFloat64 and posts via `doCallbacksGenericPointer`.
Impact: The per-frame time-series waveform has `maxAttributes_` elements in C vs always 8 in Rust. For any `maxAttributes != 8` the element count differs. (Consequence of ADP-47.)

#### ADP-50: Attribute plugin re-posts stale Val/ValSum for a missing attribute; C skips the post
Severity: Low — fix-low — **FIXED a3795c6a** (Val/ValSum post + accumulation moved inside the `Some(val)` arm so a missing/non-numeric attribute skips the post, matching C `continue`)
Rust: `attribute.rs:131-142` — when `extract_value` returns `None` it keeps the previous `ch.value` but still pushes `ParamUpdate::float64_addr` for Val and ValSum (re-posting the old value).
C: `NDPluginAttribute.cpp:72-80` — on a read error/missing attribute it `continue`s, skipping setDoubleParam(Val), the ValSum accumulation, and `callParamCallbacks(i)` for that channel.
Impact: On a frame missing the tracked attribute, C emits no monitor update for that channel; Rust emits a redundant post of the unchanged value (extra monitor traffic / spurious timestamps).

#### ADP-51: AttrPlot rejects DataSelect=0 when no attributes are tracked; C allows it
Severity: Low — fix-low — **FIXED 73243091** (`set_data_select` guard changed from `value >= 0` to `value > 0`, matching NDPluginAttrPlot.cpp:283)
Rust: `attr_plot.rs:143-145` `set_data_select` rejects `value >= 0 && (value as usize) >= attributes.len()` — so `value==0` with empty list is rejected.
C: `NDPluginAttrPlot.cpp:283-285` rejects only `value > 0 && (unsigned)value >= attributes_.size()` — `value==0` always accepted.
Impact: `caput AP_DataSelect 0` before any frame (empty attribute list) succeeds in C but errors in Rust; divergent write status and stored DataSelect.

#### ADP-52: AttrPlot is not wired into the IOC; no NDAttrPlotConfig command exists
Severity: Medium — verify
Rust: `AttrPlotProcessor` is referenced only inside `attr_plot.rs` (its own tests); no `NDAttrPlotConfig` startup command in `ioc.rs`, no runtime factory. `rg` across `crates/` finds zero production instantiations.
C: `NDPluginAttrPlot.cpp:308-318` registers `NDAttrPlotConfig` iocsh command and constructs the plugin.
Impact: A db invoking `NDAttrPlotConfig` fails to create the plugin in the Rust IOC; no AP_Data/AP_DataLabel/AP_Attribute/AP_NPts records are ever served. ADP-51/53/54/55 are latent until this is wired.

#### ADP-53: AttrPlot exposes AP_Data on every frame; C exposes it only every 1 s from a background task
Severity: Low — signoff
Rust: `attr_plot.rs:325-338` `process_array` calls `build_updates()` emitting per-block AP_Data on **every** frame.
C: `NDPluginAttrPlot.cpp:96-124` `callback_data` is invoked not from `processCallbacks` but from `ExposeDataTask::run` every `ND_ATTRPLOT_DATA_EXPOSURE_PERIOD = 1.0` s (plus once per DataSelect write); `processCallbacks` only updates AP_NPts.
Impact: AP_Data monitor cadence differs: per-frame (Rust) vs 1 Hz (C). Steady-state values match. Latent (see ADP-52).

#### ADP-54: AttrPlot unlimited-cache (cache_size=0) emits live-count-length AP_Data; C always emits cache_size
Severity: Low — signoff
Rust: `attr_plot.rs:258-276` `block_waveform` targets `cache_size` when fixed but the live `size` when `cache_size==0`.
C: `NDPluginAttrPlot.cpp:96-121` always `doCallbacksFloat64Array(tmp_arr, cache_size, …)`; C has no unlimited mode (`cache_size==0` → `max_length_==0`, modulo-by-zero, invalid config).
Impact: With a fixed cache both emit `cache_size`-length tail-padded arrays (match). The Rust unlimited mode is a Rust-only extension with no valid C counterpart — no C-observable divergence.

#### ADP-55: AttrPlot tracks at most `n_attributes`; C off-by-one tracks up to `n_attributes_+1`
Severity: Low — signoff
Rust: `attr_plot.rs:205` `names.truncate(self.n_attributes)` — caps at exactly `n_attributes`.
C: `NDPluginAttrPlot.cpp:162-164` loop `attributes_.size() <= n_attributes_` admits one extra name, but `data_` holds only `n_attributes_` buffers, so `push_data` indexes `data_[n_attributes_]` out of bounds.
Impact: For an array with more than `n_attributes_` numeric attributes, C tracks one more (or crashes on the OOB write); the Rust cap is a safety improvement. The only "observable" C difference is the extra `AP_Attribute[n_attributes]` name before the crash — signoff (C path is a latent bug).

#### ADP-56: float→int out-of-range conversion: Rust saturates, C wraps (UB)
Severity: Low — signoff
Rust: `plugin/runtime.rs:1291-1310` `cast_from_f64` uses Rust `as` (f64→iN saturates; NaN→0).
C: `NDArrayPool.cpp:378-387` `convertType` uses `(dataTypeOut)(*pDataIn++)` — plain C cast; out-of-range float→int is undefined.
Impact: In-range values truncate identically. They diverge only for out-of-range float→int when a float NDArray is read as an integer waveform; C's result is undefined/platform-dependent, so not a well-defined parity target — signoff. (int→int already uses C-cast truncation via `copy_ccast`, `runtime.rs:1218-1228`.)

#### ADP-57: StdArrays does not special-case codec/compressed arrays on read
Severity: Low — verify
Rust: `plugin/runtime.rs:1324-1346` `impl_read_array!` always reads `array.data` (typed buffer) with no codec branch; `std_arrays.rs:34-38` stores `array.clone()` unconditionally.
C: `NDPluginStdArrays.cpp:43-57,98-120` branches on `pArray->codec.empty()` — for a compressed array copies the raw compressed bytes (`compressedSize`) instead of converting, `numElements = compressedSize/bytesPerElement + 1`.
Impact: If a compressed NDArray reaches StdArrays, C emits the raw compressed byte stream; Rust emits the decompressed/typed buffer. Reachability depends on whether the Rust input can carry an undecoded codec — verify the upstream wiring.

#### ADP-58: passthrough.rs has no direct C plugin counterpart
Severity: Low — signoff
Rust: `passthrough.rs:11-43` `PassthroughProcessor` is a stub returning `ProcessResult::empty()` for not-yet-implemented plugin types; adds a `PV_NAME` Octet param only when `plugin_type == "NDPvaConfigure"`.
C: no single upstream "passthrough" plugin (`NDFileNull`/`NDFileDummy` are file-writer stubs; base `NDPluginDriver` passthrough is the framework).
Impact: No C plugin to diverge from; a deliberate placeholder, no wire-parity claim applies.

#### ADP-59: StdArrays/Attribute NDArrayCallbacks initial value — Rust defaults on, C off
Severity: Low — verify
Rust: `plugin/runtime.rs:1104` sets `ndarray_params.array_callbacks = 1` for every plugin port; no StdArrays-specific override to 0.
C: `NDPluginStdArrays.cpp:343` and `NDPluginAttribute.cpp:203` both `setIntegerParam(NDArrayCallbacks, 0)` in the constructor.
Impact: The initial `ArrayCallbacks` param a client reads for StdArrays/Attribute ports is 1 in Rust but 0 in C. Primarily an initial-param-value divergence; verify whether any downstream behavior keys off ArrayCallbacks==0 for these two plugins.

#### ADP-60: StdArrays throttle does not decrement ArrayCounter; Rust has no throttle-rollback
Severity: Low — verify
Rust: `plugin/runtime.rs:701-702` increments `array_counter` per processed array; the generic dropped-output throttle path is not shown to roll the counter back for StdArrays' per-interface throttle.
C: `NDPluginStdArrays.cpp:59-67` per-interface `throttled()` increments `NDPluginDriverDroppedOutputArrays`; `:206-211` decrements `NDArrayCounter` once if any interface throttled, so clients monitoring ArrayCounter see no bump for a throttled frame.
Impact: On a byte-rate-throttled frame, C presents ArrayCounter unchanged + DroppedOutputArrays incremented; the Rust counter/dropped accounting for this case needs verification against the decrement-on-throttle behavior.

### NDFileHDF5 writer (ADP-61..80)

#### ADP-61: Default layout is flat `/data`, not the C NeXus `/entry/instrument/detector/data` tree
Severity: High — fix
Rust: `file_hdf5.rs:284,1010-1014` `resolved_dataset_path = "data"` when no layout XML is loaded; `resolve_layout_paths` falls back to flat root.
C: `NDFileHDF5LayoutXML.cpp:43-70` `DEFAULT_LAYOUT`; `NDFileHDF5.cpp:3899-3906` loads it when the layout filename param is empty.
Impact: With NO user XML (the default mode), C writes the image at `/entry/instrument/detector/data` inside a full NeXus tree (NXentry/NXinstrument/NXdetector/NXcollection/NXdata) plus a `/entry/data/data` hardlink, an `NDAttributes` NXcollection, and a `performance` group. Rust writes a single flat dataset `data` at the root with none of the NeXus groups, NX_class attrs, or hardlink. The single largest divergence in the writer.

#### ADP-62: No NX_class / NeXus group attributes emitted for the default layout
Severity: High — fix
Rust: no built-in default layout; `build_layout_groups` runs only when `self.layout` is `Some`, and even then materialises only group nodes, never `NX_class` constant attributes (only dataset-level constant attrs, `file_hdf5.rs:1317-1347`); `for_each_dataset` (`file_hdf5.rs:1204`) visits datasets only, so a `<group><attribute>` constant is dropped.
C: `NDFileHDF5LayoutXML.cpp:45,47,49,54,60,67` (NXentry/NXinstrument/NXdetector/NXcollection/NXdata); written via `NDFileHDF5.cpp:693-695` `writeHdfAttributes(new_group, root)` when `storeAttributes==1`.
Impact: C attaches `NX_class` string attributes to the entry/instrument/detector/NDAttributes/data groups; Rust never writes any group-level constant attribute. NeXus readers will not recognise the file as NeXus.

#### ADP-63: Default ColorMode NDAttribute dataset and per-dataset ndattribute placement missing
Severity: Medium — fix
Rust: NDAttribute datasets come from the live `array.attributes` list (`file_hdf5.rs:1457-1467`) and always land in the flat `NDAttributes` group or the layout `ndattr_default` group; the layout's `<dataset source="ndattribute">` nodes are never honoured for placement.
C: `NDFileHDF5LayoutXML.cpp:55` default `<dataset name="ColorMode" source="ndattribute" ndattribute="ColorMode">`; `NDFileHDF5.cpp:2792-2800` routes a matching NDAttribute into the XML-declared dataset/group via `find_dset_ndattr`/`setDsetName`.
Impact: C writes `ColorMode` at `/entry/instrument/detector/NDAttributes/ColorMode` (the XML-pinned path) and any user `<dataset source="ndattribute">` at its declared path/name; Rust ignores this placement and writes every attribute under the single ndattr group keyed by raw name. Different on-disk dataset paths.

#### ADP-64: NDAttribute datasets omit the four NDAttr* self-describing HDF5 attributes
Severity: Medium — fix — **FIXED 03f4aaac** (`AttributeDataset` now derives `description`/`source`/`source_type` from the `NDAttribute`; `write_ndattr_descriptors` attaches `NDAttrName`/`NDAttrDescription`/`NDAttrSourceType`/`NDAttrSource` as scalar string attrs, each skipped when empty, mirroring C's per-name non-empty `writeStringAttribute`. Datatype is the port's `VarLenUnicode` scalar-string convention rather than C's fixed-length NULLTERM `H5T_C_S1`; that vlen-vs-fixed divergence is a port-wide string-attr concern, not this finding's cited defect)
Rust: `flush_attribute_datasets` (`file_hdf5.rs:1471-1551`) creates each attribute dataset with no attached HDF5 attributes.
C: `NDFileHDF5.cpp:2715` `attrNames[] = {"NDAttrName","NDAttrDescription","NDAttrSourceType","NDAttrSource"}`; values at 2785-2788; written (each only when non-empty) via `writeStringAttribute` (3019-3040) as scalar NULLTERM C strings.
Impact: C attaches up to four string HDF5 attributes to every NDAttribute dataset; Rust writes none, so a reader sees no source/description metadata.

#### ADP-65: Attribute dataset chunk default differs (C: numCapture/16K; Rust: 16)
Severity: Low — fix-low — **FIXED c78dbac3** (added `NDFileWriter::set_num_capture` default-no-op hook the controller pushes before each open; `Hdf5Writer` stores the open mode + capture target and resolves the chunk via `attribute_chunking` mirroring C `calculateAttributeChunking` — auto(0)→Single 1, else numCapture, else 16*1024; `ndattr_chunk` keeps 0 as the auto sentinel instead of clamping to 1, and the chunk is no longer clamped to the frame count since the dataset is extensible)
Rust: `ChunkConfig::ndattr_chunk` default `16` (`file_hdf5.rs:66`); chunk = `min(ndattr_chunk, n).max(1)` (`file_hdf5.rs:1497`).
C: `calculateAttributeChunking` (`NDFileHDF5.cpp:2869-2920`): param default `0` (`2324`) → uses `NDFileNumCapture`; if capture ≤ 0 → `16*1024`.
Impact: When `HDF5_NDAttributeChunk` is default (0), C chunks at numCapture (or 16384), Rust at 16 (clamped to frame count). Different chunk dimension in the DCPL (`H5Pget_chunk`); data values identical.

#### ADP-66: SZIP filter uses entropy-coding mask (4); C uses nearest-neighbor mask (32)
Severity: Medium — fix — **FIXED 77c33163** (NN mask 32; round-trips. The library-OR'd CHIP/K13 bits and the block/pixel-count cd_values libhdf5's H5Z_set_local_szip appends are not replicable through rust-hdf5's hand-built pipeline — full cd_values byte-parity vs libhdf5 remains an unverified residual)
Rust: `build_pipeline` SZIP arm `cd_values: vec![4, self.szip_num_pixels]` (`file_hdf5.rs:457`).
C: `NDFileHDF5.cpp:3372` `H5Pset_szip(this->cparms, H5_SZIP_NN_OPTION_MASK, szipNumPixels)`.
Impact: `H5_SZIP_EC_OPTION_MASK==4` (entropy) vs `H5_SZIP_NN_OPTION_MASK==32` (nearest-neighbor). Rust selects the wrong SZIP coding mode, so the compressed bytes and stored cd_values differ. (C also relies on the library to OR in CHIP/ALLOW_K13 bits and append block/pixel-count cd_values the hand-built Rust pipeline does not replicate.) Decodable but not byte-parity and a different compression result.

#### ADP-67: N-bit filter implemented as cd_values; C sets datatype precision/offset + paramless H5Pset_nbit
Severity: High — fix — **MITIGATED 8367f639; output-form parity BLOCKED by rust-hdf5 0.2.17.** The malformed-filter defect is removed: the old `cd_values: vec![precision, offset]` was shorter than `apply_nbit`'s 4-element minimum (so precision>0 errored the write) and was dropped entirely at the default precision==0 (so a default nbit request produced no compression). Faithful N-bit output requires a reduced-precision dataset datatype message (C `H5Tset_precision`/`H5Tset_offset`), which rust-hdf5 0.2.17's high-level `DatasetBuilder<T: H5Type>` cannot emit — `T::hdf5_type()` is a static fn with no runtime precision input, and the low-level `create_chunked_dataset_with_pipeline(datatype: DatatypeMessage)` is not reachable through `H5File`. N-bit now degrades to lossless uncompressed (data round-trips exactly; SWMR flags `compression_dropped`). **Remaining (UNFIXED, blocked): true N-bit byte parity** needs a rust-hdf5 API addition (expose a reduced-precision `FixedPoint` datatype + the nbit parameter tree `[nparms, need_not_compress, d_nelmts, class, size, order, precision, offset]`) or a dep bump; not closable in source.
Rust: `build_pipeline` NBIT arm adds `Filter { id: FILTER_NBIT, cd_values: vec![precision, offset] }` (`file_hdf5.rs:497-509`); dataset datatype left full-width.
C: `NDFileHDF5.cpp:3355-3357` `H5Tset_precision(datatype, nbitPrecision); H5Tset_offset(datatype, nbitOffset); H5Pset_nbit(cparms);` — the N-bit filter takes no client cd_values; bit packing is driven by the dataset datatype's precision/offset.
Impact: In C the on-disk datatype carries reduced precision/offset and nbit packs to it (observably narrower datatype). In Rust the datatype stays full-width and an nbit filter is written with a `[precision, offset]` cd_values pair the standard nbit filter does not interpret — datatype, filter message, and packed bytes all differ. C nbit precision default is `8` (`NDFileHDF5.cpp:2340`); Rust defaults precision `0` and drops the filter entirely when precision==0, so a default-config nbit request produces no compression at all in Rust.

#### ADP-68: BLOSC cd_values layout differs from C's reserved-slot convention
Severity: Medium — verify
Rust: `build_pipeline` BLOSC arm writes 7 cd_values `[2, 2, element_size, 0, level, shuffle, compressor]` (`file_hdf5.rs:485-493`).
C: `NDFileHDF5.cpp:3387-3391` declares `cds[7]`, fills only `cds[4]=level, cds[5]=shuffle, cds[6]=compressor`, leaves slots 0-3 uninitialised (comment: "0 to 3 inclusive reserved"), calls `H5Pset_filter(FILTER_BLOSC, MANDATORY, 7, cds)`.
Impact: C leaves cds[0..3] for the blosc plugin to fill at runtime (format version, blosc version, typesize, blocksize); Rust hardcodes `[2,2,element_size,0]`. The stored filter message's first four cd_values differ between a C file (plugin-populated) and the Rust file. Whether the Rust file still decompresses depends on the reader's blosc plugin tolerating the authored typesize/blocksize. Verify because the exact C-side plugin-written values are runtime-dependent.

#### ADP-69: BLOSC default compressor/level/shuffle defaults differ
Severity: Low — fix-low — **FIXED 44d05a47** (default `blosc_shuffle_type` 0→1 = byte shuffle, matching C `NDFileHDF5.cpp:2344`; compressor 0 and level 5 already matched)
Rust: defaults `blosc_shuffle_type=0`, `blosc_compressor=0`, `blosc_compress_level=5` (`file_hdf5.rs:261-263`).
C: `NDFileHDF5.cpp:2344-2346` `bloscShuffleType=1`, `bloscCompressor=0`, `bloscCompressLevel=5`.
Impact: C's default BLOSC shuffle is `1` (byte shuffle); Rust's is `0` (none). A default-config BLOSC write produces a different `cds[5]` and a different compressed byte stream. Level (5) and compressor (0) match.

#### ADP-70: On-disk numeric datatype byte order is hardcoded LE; C uses native-endian
Severity: Low — signoff
Rust: rust-hdf5 emits every fixed/floating datatype message with `ByteOrder::LittleEndian` hardcoded (`rust-hdf5-0.2.17/src/format/messages/datatype.rs:133-228`); the writer also LE-serialises data (`nd_buffer_to_le_bytes`, `file_hdf5.rs:1666-1679`).
C: `typeNd2Hdf` (`NDFileHDF5.cpp:3484-3524`) and `typeAsHdf` (`NDFileHDF5AttributeDataset.cpp:327-363`) use `H5T_NATIVE_*` — records the writing machine's byte order.
Impact: On a little-endian host (the common case) identical. They diverge only on a big-endian host: C records BE, Rust still records LE (byte-swapping data to match). On-disk identical on LE hardware; signoff since BE EPICS IOCs are vanishingly rare and the Rust behaviour is arguably more portable.

#### ADP-71: Detector dataset omits C's NDArrayNumDims/DimOffset/DimBinning/DimReverse + signal attributes
Severity: Medium — fix — **PARTIALLY FIXED 9ef09199** (`create_primary_dataset` now writes `NDArrayNumDims` (scalar int32, always) and `NDArrayDimOffset`/`NDArrayDimBinning`/`NDArrayDimReverse` (scalar int32) for the 1-D case, native dim order. **UNFIXED, blocked:** for `ndims>1` C writes the three `Dim*` as 1-D int32 *arrays* of length ndims; rust-hdf5 0.2.17's `AttrBuilder` is scalar-only (`shape()` ignores its arg, "we only support scalar attributes") so the multi-dim array case cannot be emitted in source. **Deferred to ADP-61/62/63:** the default-layout `signal=1` attr is part of the NeXus default-layout feature, not this finding)
Rust: `create_primary_dataset` attaches only `NDArrayDataType`, `HDF5_fillValue`, `HDF5_nRowChunks/nColChunks/nFramesChunks/nExtraDims`, `HDF5_extraDimSize*/Name*` (`file_hdf5.rs:1272-1316`) plus layout constant attrs.
C: `writeDefaultDatasetAttributes` (`NDFileHDF5.cpp:3684-3739`) attaches `NDArrayNumDims`, `NDArrayDimOffset`, `NDArrayDimBinning`, `NDArrayDimReverse` (int32, comma-source, OnFileOpen) to every detector dataset; the default layout adds `signal=1` (`NDFileHDF5LayoutXML.cpp:51`).
Impact: A C-written detector dataset carries five extra HDF5 attributes the Rust port never writes; conversely Rust writes a set of `HDF5_*`/`NDArrayDataType` attrs C does not. The attribute name/value sets a reader observes are disjoint except by accident.

#### ADP-72: Performance dataset chunk dim differs (C `[chunking,5]`; Rust `[1,5]`)
Severity: Low — fix-low — **FIXED 27950c3d** (`flush_performance_dataset` now chunks `[chunking,5]` reusing `attribute_chunking` (the same C `calculateAttributeChunking` value, depends on ADP-65) and bands the writes through `write_chunked_buffer`; extent `[n,5]`, type, and column meanings unchanged)
Rust: `flush_performance_dataset` creates `timestamp` with `chunk(&[1,5])` (`file_hdf5.rs:1588`).
C: `NDFileHDF5.cpp:2645-2647` `chunk[2] = {chunking, 5}` where `chunking = calculateAttributeChunking(...)` (numCapture or 16K).
Impact: The `[N,5]` shape, `H5T_NATIVE_DOUBLE` type, and five column meanings match C. Only the chunk dimension differs: C chunks deep, Rust one row per chunk (`H5Pget_chunk`); values identical.

#### ADP-73: Performance dataset group default differs when no layout (`performance` vs root)
Severity: Low — signoff
Rust: with no layout, performance lands in a flat `performance` group (`file_hdf5.rs:1578-1581`).
C: with the default layout the perf group is `/entry/instrument/performance`; if no perf group found and `auto_ndattr_default` true, C falls back to `timestamp` at root (`NDFileHDF5.cpp:2622-2626`).
Impact: Consequence of ADP-61. C default `/entry/instrument/performance/timestamp`; Rust `/performance/timestamp`. Folds into the ADP-61 fix; the group name itself (`performance`) is right, only the parent tree is missing.

#### ADP-74: Extra-dim dataspace collapses N extra dims into one leading axis; C builds multiple unlimited axes
Severity: Medium — verify
Rust: `primary_layout` produces rank `1 + frame_dims.len()`; with extra dims the single leading axis is fixed at `product(extraDimSize)` and never extended; without extra dims one leading axis is unlimited (`file_hdf5.rs:544-582`).
C: dataset-level `configureDims` (`NDFileHDF5Dataset.cpp:59,88,101-105`) builds rank `pArray->ndims + (nExtraDims+1)`, marking EACH leading extra-dim axis `H5S_UNLIMITED`.
Impact: For `HDF5_nExtraDims=N`, C writes a rank `ndims+N+1` dataset with separate unlimited extra-dim axes; Rust collapses all extra dims into ONE leading axis of size `product(sizes)`, rank `1+ndims`, recording the intended shape only as `HDF5_extraDimSize*` attrs. A reader sees a different rank/shape with extra dims configured. Verify because the C multi-extra-dim path is gated on `multiFrameFile`/dimAttDataset modes; the common single-extra-frame-dim case matches.

#### ADP-75: Chunk-selection rule is parity-faithful (clean)
Severity: Low — signoff
Rust: `clamp_chunk` returns full dim when `auto || requested==0 || requested>dim` (`file_hdf5.rs:586-592`); frame-axis chunk = `n_frames_chunks.max(1)` default 1.
C: image-axis loop `NDFileHDF5.cpp:3254-3257` clamps to dim then full-dim on auto/<1; frame-axis `3267-3271` default 1.
Impact: None — too-large clamps to dim, auto/0 → full dim, frame axis default 1. Parity-clean; recorded as positive verification.

#### ADP-76: Fill value handling is parity-faithful (always set, default 0) (clean)
Severity: Low — signoff
Rust: `create_primary_dataset` always calls `.fill_value(fill as $t)`, default `fill_value=0.0` (`file_hdf5.rs:267,1264`).
C: `NDFileHDF5.cpp:3882` `H5Pset_fill_value(cparms, datatype, ptrFillValue)` unconditionally; default 0.
Impact: None — both always set a fill value defaulting to 0 cast to the dataset type. Parity-clean. (The extra `HDF5_fillValue` float64 attr Rust writes is covered under ADP-71.)

#### ADP-77: NDAttribute string dataset stored as `[n,256]` byte array; C stores `[n]` of a fixed 256-byte H5T_C_S1 string
Severity: Medium — fix — **FIXED 05bc160d** (`FixedStr256` newtype implements `rust_hdf5::types::H5Type` emitting `DatatypeMessage::fixed_string(256)` = `H5Tcopy(H5T_C_S1)`+`H5Tset_size(256)`, NULLTERM/ASCII; string dataset now `new_dataset::<FixedStr256>().shape([n]).chunk([chunk]).max_shape([None])` — rank-1 fixed-length string, matching C)
Rust: string attribute dataset `new_dataset::<u8>().shape([n, es]).chunk([chunk, es])` with `es=256` (`file_hdf5.rs:1532-1547`) — a 2-D uint8 array.
C: `NDFileHDF5AttributeDataset.cpp:321-323` `datatype_ = H5Tcopy(H5T_C_S1); H5Tset_size(256)` with rank 1 (`configureDims` rank_=1, `:234,257`) — a 1-D dataset of a fixed-length string type.
Impact: For a string-valued NDAttribute, C writes a 1-D `[nframes]` dataset of 256-byte fixed-length C strings; Rust writes a 2-D `[nframes,256]` `H5T_STD_U8LE` array. Different rank, different element datatype (string vs uint8). HDF5 string tooling will not recognise the Rust version as strings.

#### ADP-78: NDAttribute Int64/UInt64 width and Undefined-type handling
Severity: Low — signoff
Rust: `AttributeDataset::element_size`/`push` handle Int64/UInt64 as 8-byte; `String` fixed 256 (`file_hdf5.rs:101-148`). An attribute absent in a frame pushes `NDAttrValue::Undefined` which `as_i64/as_f64` coerce to 0 (`file_hdf5.rs:1444-1449`).
C: `NDFileHDF5AttributeDataset.cpp:351-355` Int64/UInt64 → `H5T_NATIVE_INT64/UINT64`; undefined/default type → `H5T_NATIVE_FLOAT` with fill `epicsNAN`, skips the write (`:366-370,160`).
Impact: Numeric widths and the 256-byte string size match. Divergence: for an Undefined-typed attribute, C creates a float dataset and leaves missing frames NaN; Rust resolves type from the first concrete value and writes 0 for missing frames. The dtype for a genuinely-undefined attribute and the missing-frame sentinel (0 vs NaN) differ. Low because undefined-typed attributes are rare — signoff.

#### ADP-79: Layout `<attribute source="ndattribute">` on groups/datasets and `when` (OnFileOpen/Close) not materialised
Severity: Medium — fix
Rust: only `LayoutSource::Constant` attributes are materialised, on datasets only (`file_hdf5.rs:1206-1212,956-960`); `ndattribute`-sourced `<attribute>` nodes are skipped, and `LayoutWhen` is parsed (`hdf5_layout.rs:46-64`) but never consulted.
C: `NDFileHDF5.cpp:553-632` `storeOnOpenCloseAttribute` writes an `<attribute source="ndattribute">` as an HDF5 attribute on its group/dataset — OnFileOpen → first value, OnFileClose → last value (`:302`, `:1662`).
Impact: A layout with `<attribute source="ndattribute" when="OnFileClose">` produces an on-disk HDF5 attribute carrying the live NDAttribute value in C; Rust writes nothing. Observable missing attributes for any non-constant layout attribute. (Constant attributes are unaffected — C ignores `when` for constants, `NDFileHDF5LayoutXML.cpp:413-430`, and so does Rust, correctly.)

#### ADP-80: `<global name="detector_data_destination">` parsed but never used to route the detector dataset
Severity: Low — verify
Rust: `Hdf5Layout::detector_data_destination` is parsed (`hdf5_layout.rs:131-133,432-436`) but never read by `resolve_layout_paths` or any writer; the detector dataset is always placed by `det_default`/first-detector-source (`hdf5_layout.rs:183-199`).
C: `NDFileHDF5.cpp:498` reads `get_global("detector_data_destination")` to select which NDAttribute names the destination dataset for detector data.
Impact: When a layout uses `<global name="detector_data_destination" ndattribute="..."/>` to route detector frames to one of several datasets by attribute value, C honours it; Rust ignores it and always writes to the static `det_default`. Observable only for layouts using this dynamic-destination feature (uncommon) — verify.

### NeXus / Magick writers + PVA NTNDArray converter (ADP-81..95)

#### ADP-81: PVA NTNDArray wire structure is byte-faithful to the canonical NT definition (clean)
Severity: Low — signoff
Rust: `crates/epics-pva-rs/src/nt/nd_array.rs:395-415` `nt_nd_array_desc()`.
C: `pvxs/src/nt.cpp:196-251` `NTNDArray::build()`.
Impact: None. Top-level field order (`value, codec, compressedSize, uncompressedSize, uniqueId, dataTimeStamp, alarm, timeStamp, dimension, attribute`), struct IDs, value-union variant ordering (signed-first then unsigned), `dimension_t` member set (`size/offset/fullSize/binning/reverse`), NTAttribute member set, and `time_t` all match exactly. A pvData NTNDArray consumer decodes the Rust frame identically.

#### ADP-82: PVA value-union member chosen per NDDataType matches C fromValue (clean)
Severity: Low — signoff
Rust: `crates/ad-plugins-rs/src/pva.rs:127-138`, `nd_array.rs:90-104,136-151`.
C: `ntndArrayConverter.cpp:433-454` `fromValue`.
Impact: None. Each NDDataType selects its type-specific arm exactly as C's `switch(src->dataType)`; compressed emits raw bytes under `ubyteValue` matching C's `fromValue<PVUByteArray>`; union selector indices agree.

#### ADP-83: PVA codec.parameters original-type integer matches C NDDataTypeToScalar (clean)
Severity: Low — signoff
Rust: `crates/ad-plugins-rs/src/pva.rs:257-270` `nd_data_type_to_scalar`; `crates/epics-pva-rs/src/pvdata/scalar.rs:11-23`.
C: `ntndArrayConverter.cpp:25-36` `NDDataTypeToScalar[]`, written at `416-419`.
Impact: None. C writes the pvData ScalarType ordinal per array; the table maps Int64→pvLong(4), UInt64→pvULong(8) (its index-6/7 comments are typos, the values are correct). Rust reproduces the same integer per type with matching ScalarType discriminants.

#### ADP-84: PVA uncompressedSize is the original byte count on both branches (clean)
Severity: Low — signoff
Rust: `crates/ad-plugins-rs/src/pva.rs:116-138`.
C: `ntndArrayConverter.cpp:404-411` (`uncompressedSize = arrayInfo.totalBytes` always).
Impact: None. C always publishes `uncompressedSize = nElements*bytesPerElement` of the ORIGINAL type; Rust computes `num_elements * original_type.element_size()`. Coincide in the common path (see ADP-85 for the over-allocation edge).

#### ADP-85: PVA uncompressed compressedSize uses recomputed totalBytes, not the NDArray field
Severity: Low — signoff
Rust: `crates/ad-plugins-rs/src/pva.rs:133-137` (uncompressed branch → `compressed_size = uncompressed_size`).
C: `ntndArrayConverter.cpp:407,410` `compressedSize = src->compressedSize`.
Impact: For an uncompressed array C copies the NDArray's own `compressedSize` (= `dataSize`, normally `totalBytes` but LARGER when a driver over-allocates, `NDArray.cpp:58`). In that rare case C's wire `compressedSize` exceeds `uncompressedSize`; Rust always emits exactly `uncompressedSize`. Requires a non-default over-allocating driver — observable-but-marginal, signoff.

#### ADP-86: PVA dimension[].binning clamps 0→1 where C serializes the raw value
Severity: Low — fix-low — **FIXED cb41b18d**
Rust: `crates/ad-plugins-rs/src/pva.rs:151` `binning: d.binning.max(1) as i32`.
C: `ntndArrayConverter.cpp:471` `…->put(src->dims[i].binning)` (no clamp).
Impact: C copies `dims[i].binning` verbatim into the wire field; Rust forces a minimum of 1. If an upstream source sets `binning = 0`, the C wire shows `0` and Rust shows `1` — an observable `dimension[].binning` difference. `NDDimension::new` default is already 1, so this manifests only when something explicitly sets binning to 0 (non-physical) — low severity. The only wire divergence in the PVA group.

#### ADP-87: PVA dataTimeStamp/timeStamp dual-source + POSIX offset match C (clean)
Severity: Low — signoff
Rust: `crates/ad-plugins-rs/src/pva.rs:160-161,221-246`.
C: `ntndArrayConverter.cpp:477-503` `fromDataTimeStamp`/`fromTimeStamp`.
Impact: None. `dataTimeStamp` from the float `NDArray::timeStamp` (floor secs, frac→ns), `timeStamp` from integer `epicsTS`; both add `POSIX_TIME_AT_EPICS_EPOCH`; `userTag` 0. Rust reproduces both sources and the offset.

#### ADP-88: PVA NTAttribute fields incl. null-variant handling match C (clean)
Severity: Low — signoff
Rust: `crates/ad-plugins-rs/src/pva.rs:163-185,275-310`.
C: `ntndArrayConverter.cpp:544-591` `fromAttributes`/`fromAttribute`/`fromUndefinedAttribute`.
Impact: None. Per attribute C sets only `name`, `descriptor`, `source`, `sourceType` (raw int), and the `value` any (typed scalar or null for `NDAttrUndefined`). Rust matches: defined → tagged scalar, `Undefined` → `VariantValue::null()`, per-attribute timeStamp/alarm/tags stay at NT defaults (C never populates them).

#### ADP-89: PVA codec.name and uniqueId match C (clean)
Severity: Low — signoff
Rust: `crates/ad-plugins-rs/src/pva.rs:127-142,193,248-250`.
C: `ntndArrayConverter.cpp:420,231-232`.
Impact: None. Uncompressed → empty `codec.name`; compressed → the codec's name. `uniqueId` copied directly; `alarm` fixed `NO_ALARM`/0/0.

#### ADP-90: NeXus group/dataset layout and dtype mapping match C processNode (clean)
Severity: Low — signoff
Rust: `crates/ad-plugins-rs/src/file_nexus.rs:263-308,442-544,719-748`.
C: `NDFileNexus.cpp:205-475`.
Impact: None for the on-disk tree. The 44 NeXus base-class group names + the `UserGroup` rule match C's list/test; group naming (name attr else tag, NX_class = tag) matches; the dtype switch maps each NDDataType to the same HDF5 type; CONST/ND_ATTR node handling parallels C.

#### ADP-91: NeXus NX_class is a true group attribute; C writes it via NXmakegroup (clean)
Severity: Low — signoff
Rust: `crates/ad-plugins-rs/src/file_nexus.rs:391-400,470`.
C: `NDFileNexus.cpp:254-255` `NXmakegroup(handle, name, class)`.
Impact: None. C's NXmakegroup records the class via the napi group-class mechanism (an NX_class HDF5 group attribute); Rust writes NX_class as a real group attribute. A NeXus reader sees the same class.

#### ADP-92: NeXus capture/stream leading frame dimension matches C slab layout (clean)
Severity: Low — signoff
Rust: `crates/ad-plugins-rs/src/file_nexus.rs:703-856`.
C: `NDFileNexus.cpp:400-411,498-536`.
Impact: None observable in shape/contents. C prepends a frame axis (`rank+1`, leading dim `numCapture`) writing each frame as a slab at `slabOffset[0]=imageNumber`; Rust creates a `[1,…]` chunked dataset extended per frame to `[N,…]`. Both index frames on the leading axis with identical reversed per-frame dims. C pre-sizes to numCapture, Rust grows to actual count — differs only if capture ends early (C leaves trailing uninitialized frames), a benign capacity-vs-actual difference.

#### ADP-93: NeXus per-frame uniqueId/timeStamp datasets and DTYPE attribute are Rust additions absent from C output
Severity: Medium — signoff
Rust: `crates/ad-plugins-rs/src/file_nexus.rs:32,753-757,787-818` (`NDArrayDataType` attr, `uniqueId`/`timeStamp` datasets).
C: `NDFileNexus.cpp` — no such datasets/attributes are written.
Impact: The Rust NeXus file contains extra objects C never emits (an `NDArrayDataType` i32 attr on the data dataset, sibling `uniqueId` i32[] and `timeStamp` f64[] datasets). C writes per-frame provenance only if the template contains nodes for it; the built-in/default file has none. A byte-for-byte diff against C differs (extra datasets/attrs), but no C-written object is missing or wrong — additive. Recorded as a deliberate additive divergence for sign-off (backs lossless `read_file` round-tripping, which C's reader does not implement).

#### ADP-94: NeXus built-in /entry/data NXdata group + hard-link to detector dataset has no C analog (template-only in C)
Severity: Low — signoff
Rust: `crates/ad-plugins-rs/src/file_nexus.rs:642-666,775-785`.
C: `NDFileNexus.cpp` — all placement is template-driven; no built-in hierarchy.
Impact: C's NDFileNexus produces NO structure without a loaded XML template; Rust adds a built-in `entry/instrument/detector` + `entry/data` hierarchy (with a hard link) for the no-template case. When a template IS loaded, Rust's `process_template` drives placement exactly like C and the built-in hierarchy is not created (`nxdata_group_path = None`). No divergence in template mode; in no-template mode C writes nothing usable, so no C output to diverge from.

#### ADP-95: Magick RGB2/RGB3 correct where C writes uninitialized pixels (C bug); Magick infers RGB from dims where C requires the ColorMode attribute
Severity: Medium — signoff
Rust: `crates/ad-plugins-rs/src/file_magick.rs:81-93,131-147,279-333`.
C: `NDFileMagick.cpp:41-96` (`openFile`), `125-146` (`writeFile`).
Impact: Two intentional output-form deviations, both improvements over C:
- (a) RGB2/RGB3: C `writeFile`'s `switch(colorMode)` has EMPTY `case NDColorModeRGB2/RGB3:` bodies (136-139) — `image.read()` is never called, so C writes an uninitialized/empty Image. Rust converts RGB2/RGB3 to interleaved RGB1 (`convert_rgb_layout`) and writes correct pixels.
- (b) Color-mode source: C `openFile` reads ONLY the `ColorMode` attribute (default Mono); a 3-D `[3,X,Y]` array with no attribute fails C's RGB conditions and returns `asynError` (no file). Rust `color_mode` falls back to inferring RGB1/RGB2/RGB3 from a size-3 dimension, producing a file where C produces none.
Both deliberate corrections; recorded for sign-off. The storage-type/pixel-depth per NDDataType and the Mono grayscale path otherwise match C.
