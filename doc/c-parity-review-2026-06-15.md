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
| OPT-T5 (table sqrt/asin domain clamps vs C bare NaN/Inf) | signoff | Signoff — keep Rust guards (user call) | — |
| OPT-T6 (table speed-ratio NaN guard vs C 0/0 poison) | signoff | Signoff — keep Rust guard (user call) | — |
| OPT-3 (orient invertArray x/det vs x*(1/det) de-precision) | signoff | Signoff — keep Rust precision (user call) | — |
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

STD-1/2/3 share one structural root (single-owner OUTL-write flag set only by
`do_pid`), so they land in one commit. STD-7/8 are signoff (see tally).

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

#### ADC-5: NDDimensions int32-array post carries `ndims` elements, not `ND_ARRAY_MAX_DIMS` (10)
Severity: Medium — fix-low
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

#### ADC-8: `pool.convert` binning sums in f64 then casts once; C casts each element to the output type and accumulates there
Severity: Low — verify (unwired: wired ROI does pure cropping, no binning)
Rust: `crates/ad-core-rs/src/ndarray_pool.rs:516-543` accumulates in f64 then `out=sum as T` (saturates).
C: `NDArrayPool.cpp:460-466` `*pDOut += (dataTypeOut)*pDIn` (output-type arithmetic, integer wrap).
Impact: a binning sum past the int range — C wraps, Rust saturates. Latent.

#### ADC-9: `pool.convert` rejects offset+size overrun; C does not bound-check
Severity: Low — verify (unwired)
Rust: `crates/ad-core-rs/src/ndarray_pool.rs:450-455` returns `InvalidDimensions`.
C: `NDArrayPool.cpp:602-737` validates only `size/binning>0`; reads past the region.
Impact: error-vs-output, latent.

#### ADC-10: `CodecName` enum carries `Zlib`/`LZ4HDF5` not in C `codecName[]`, with a different ordinal order — STRUCTURAL CAUSE of ADP-11
Severity: Low (enum) / High (the ordinal shift it causes, see ADP-11) — fix (fold into ADP-11)
Rust: `crates/ad-core-rs/src/codec.rs:5-31` — 7 variants; `as_str` emits `"zlib"`/`"lz4hdf5"`.
C: `ADApp/ADSrc/Codec.h:4-18` — `{"","jpeg","blosc","lz4","bslz4"}`, `NDCODEC_{NONE=0,JPEG=1,BLOSC=2,LZ4=3,BSLZ4=4}`.
Impact: the four real names round-trip; the extra variants + the ad-plugins ordinal map (ADP-11) cause `COMPRESSOR=2/3/4` to select the wrong codec.

#### ADC-11: file-name NDAttribute path stringifies numeric attributes; C `getValue(NDAttrString)` errors and ignores them
Severity: Low — verify
Rust: `crates/ad-core-rs/src/plugin/file_controller.rs:203,244`, `plugin/file_base.rs:239` use `as_string()` (renders numeric → decimal).
C: `NDPluginFile.cpp:548,382` call `getValue(NDAttrString,…)`; `NDAttribute.cpp:349-361` returns ND_ERROR for a non-string attribute.
Impact: a misconfigured numeric filename attribute changes the output filename in Rust, ignored in C. Edge (non-conformant typing).

Clean in ad-core (verified): `ndarray.rs` getInfo layout, pool alloc/release/free-list/THRESHOLD 1.5, attributes source mapping + copy_from, pixel_cast round+clamp, color_layout, timestamp epoch offset; runtime queue-full/compression-drop/QueueFree/MaxByteRate/ArrayCounter/ColorMode-BayerPattern/SortBuffer.

### ad-plugins-rs (ADP)

#### ADP-1: Stats centroid higher moments from raw 2-D pixels, not threshold projection profiles
Severity: High — fix
Rust: `crates/ad-plugins-rs/src/stats.rs:493-524` accumulates mu20/mu02/mu11/m30..m04 per raw pixel.
C: `NDPluginStats.cpp:224-241` — M20/M30/M40 from `profileX[profThreshold]`, M02/M03/M04 from `profileY[profThreshold]`; only M11 (`:215`) is a raw cross-sum.
Impact: SIGMAXY/ECCENTRICITY/ORIENTATION diverge even at threshold 0; all marginal moments diverge for centroidThreshold>0.

#### ADP-2: ColorConvert RGB→Mono luminance vs `(R+G+B)/3` (= ADC-6, fix in color.rs)
Severity: High — fix
Rust: `crates/ad-core-rs/src/color.rs:131-132`; wired by `color_convert.rs:437`.
C: `NDPluginColorConvert.cpp:393,462,533`.
Impact: every non-gray RGB→Mono pixel differs.

#### ADP-3: ColorConvert false-color uses a generated jet LUT, not Rainbow/Iron tables
Severity: High — fix
Rust: `crates/ad-plugins-rs/src/color_convert.rs:270-281,414-418` — any nonzero falseColor → jet table (index 0 → (0,0,127)); 1-vs-2 ignored.
C: `NDPluginColorConvert.cpp:62-77` selects RainbowColor (1) / IronColor (2) from `colorMaps.h`; Rainbow[0]=(0,0,0).
Impact: every false-color output pixel differs; Iron mode is not distinct.

#### ADP-4: ColorConvert Bayer demosaic interpolates the image border; C leaves non-native channels 0
Severity: High — fix
Rust: `crates/ad-plugins-rs/src/color_convert.rs:51-191` interpolates every pixel incl. the 1-px border.
C: `NDPluginColorConvert.cpp:305` gates interpolation on interior; border keeps 2 channels at 0.
Impact: the one-pixel border of every demosaiced RGB output differs.

#### ADP-5: Process clip order reversed (C high-then-low, Rust low-then-high)
Severity: High — fix
Rust: `crates/ad-plugins-rs/src/process.rs:409-413` low-clip then high-clip.
C: `NDPluginProcess.cpp:175-176` high-clip then low-clip.
Impact: when the two thresholds cross, per-pixel output differs (v=200, high=100→10, low=50→999: C 999, Rust 10).

#### ADP-6: Process auto-offset-scale transforms the trigger frame; C only measures it
Severity: High — fix
Rust: `crates/ad-plugins-rs/src/process.rs:348-351` runs auto_offset_scale at stage 0b and scales the same frame.
C: `NDPluginProcess.cpp:164-178,238-249` outputs raw that frame, applies new scaling from the next.
Impact: the trigger frame's whole array diverges.

#### ADP-7: Process valid background/flat-field never invalidated on element-count change
Severity: High — fix
Rust: `crates/ad-plugins-rs/src/process.rs:160,171,394,400,601-620` keeps valid_*=true permanently; applies over `min(n, bg.len())`.
C: `NDPluginProcess.cpp:120-130` recomputes validBackground/validFlatField from `nElements==nBackgroundElements` each frame and NULLs the pointer on mismatch.
Impact: after an input-size change, C posts VALID_*=0 and skips; Rust posts 1 and applies a partial-prefix op. Status params and array diverge.

#### ADP-8: TimeSeries per-point average kept as f64; C truncates to the integer element type before dividing
Severity: High — fix
Rust: `crates/ad-plugins-rs/src/time_series.rs:270-274` `average_store[i]/divisor` in f64.
C: `NDPluginTimeSeries.cpp:191` `(epicsType)averageStore_[signal]/numAveraged_`.
Impact: integer source, numAverage>1: UInt8 200,200,200 → C 29, Rust 200. Waveform values diverge (C can wrap).

#### ADP-9: FFT processes 2-D input as per-row 1-D FFTs; C does a full 2-D FFT
Severity: High — fix
Rust: `crates/ad-plugins-rs/src/ioc.rs:196` hardcodes `Rows1D`; `fft.rs:382-439` → dims `nFreqX×height`; the `Full2D` path is never selected.
C: `NDPluginFFT.cpp:298-315,369-370` selects rank from ndims → `computeFFT_2D` → `nFreqX×nFreqY`.
Impact: every 2-D input yields different dims AND magnitudes.

#### ADP-10: Overlay shape ordinals Text/Ellipse swapped vs the C enum
Severity: High — fix
Rust: `crates/ad-plugins-rs/src/overlay.rs:466-499` maps `2=Ellipse, 3=Text`.
C: `NDPluginOverlay.h:9-13` `Cross=0,Rectangle=1,Text=2,Ellipse=3`.
Impact: `OVERLAY_SHAPE=2/3` draws the wrong shape vs C.

#### ADP-11: Codec COMPRESSOR ordinal mapping diverges (extra zlib/lz4hdf5 shift)
Severity: High — fix
Rust: `crates/ad-plugins-rs/src/codec.rs:1080-1088` maps `1=JPEG,2=Zlib,3=Blosc,4=LZ4,5=LZ4HDF5,6=BSLZ4` (the comment `:1078` mis-states the C ordinals).
C: `Codec.h:12-18` `NONE=0,JPEG=1,BLOSC=2,LZ4=3,BSLZ4=4`.
Impact: `COMPRESSOR=2` → Blosc in C, Zlib in Rust; `=3` → LZ4 vs Blosc; `=4` → BSLZ4 vs LZ4. Different codec + bytes. (Structural cause: ADC-10.)

#### ADP-12: JPEG RGB2/RGB3 written with wrong dims and as grayscale
Severity: Medium — fix
Rust: `crates/ad-plugins-rs/src/file_jpeg.rs:81-105` converts to RGB1 but leaves the stale `ColorMode=RGB2/RGB3` attribute; `ndarray.rs:407-419` then mis-reads dims.
C: `NDFileJPEG.cpp:67-78,158-167` width=dims[0], JCS_RGB, re-interleaves.
Impact: RGB2 `[x=5,c=3,y=4]` → JPEG SOF width=3,height=4,1 grayscale component. Every RGB2/RGB3 JPEG wrong.

#### ADP-13: Stats centroid/profiles computed for ndims>2; C rejects them
Severity: Medium — fix
Rust: `crates/ad-plugins-rs/src/stats.rs:970-978,992` gates only color_size/ndims>=2.
C: `NDPluginStats.cpp:205,338` return asynError for ndims>2.
Impact: 3-D mono array — Rust overwrites CENTROID/SIGMA/PROFILE/CURSOR; C leaves them stale.

#### ADP-14: Stats profile/cursor index not clamped to last valid line; Rust emits zeros
Severity: Medium — fix
Rust: `crates/ad-plugins-rs/src/stats.rs:845-878,1013-1020` out-of-range → all-zero profile, CURSOR_VAL=0.
C: `NDPluginStats.cpp:341-362` clamps `MAX(.,0)`/`MIN(.,size-1)` → edge row/col/pixel.
Impact: hot pixel at the far edge / cursor beyond image — Rust zeros, C edge pixels.

#### ADP-15: Stats histogram upper-boundary value clamped into the last bin; C counts it as above
Severity: Medium — fix
Rust: `crates/ad-plugins-rs/src/stats.rs:733-735,758-760` `.min(hs-1)` clamps a top-edge value into the last bin.
C: `NDPluginStats.cpp:42-54` `if (bin>histSize-1 || value>histMax) histAbove++`.
Impact: last-bin count, HIST_ABOVE, HIST_ENTROPY diverge.

#### ADP-16: ROIStat out-of-range/zero ROI returns zeros; C clamps to one edge pixel
Severity: Medium — fix
Rust: `crates/ad-plugins-rs/src/roi_stat.rs:218-222` offset>=size/zero → all-zero.
C: `NDPluginROIStat.cpp:241-260` clamps to ≥1 pixel and writes back clamped geometry.
Impact: per-ROI values and geometry readbacks diverge.

#### ADP-17: ROIStat 1-D background includes nonexistent Y-edges
Severity: Medium — fix
Rust: `crates/ad-plugins-rs/src/roi_stat.rs:288-309` always treats ROI as 2-D (4 edges).
C: `NDPluginROIStat.cpp:57-79` — ndims==1 background is only the 2 X-end strips.
Impact: 1-D ROI NET diverges.

#### ADP-18: ROI 3-D RGB path ignores the requested output dataType
Severity: Medium — fix
Rust: `crates/ad-plugins-rs/src/roi.rs:266` builds output with `src.data.data_type()`, ignoring `config.data_type` (the 2-D path `:414` applies it).
C: `NDPluginROI.cpp:144,166-174` converts to the requested type for RGB and mono.
Impact: 3-D RGB ROI with ROI_DATA_TYPE set — wrong output type/byte width.

#### ADP-19: ROI single-color selection not collapsed; ColorMode not forced Mono
Severity: Medium — fix
Rust: `crates/ad-plugins-rs/src/roi.rs:138-273` ignores collapse_dims, keeps size-1 color dim with RGB ColorMode.
C: `NDPluginROI.cpp:180-215` forces collapseDims, ColorMode=Mono, removes size-1 dims.
Impact: dim count, ColorMode readback, shape diverge.

#### ADP-20: Overlay Cross collapses independent SizeX/SizeY into one square
Severity: Medium — fix
Rust: `crates/ad-plugins-rs/src/overlay.rs:188-217,467-471` uses `max(size_x,size_y)` for both arms.
C: `NDPluginOverlay.cpp:95-116` independent arms.
Impact: SizeX≠SizeY draws different pixels.

#### ADP-21: Overlay Rectangle one pixel too narrow/short (exclusive vs inclusive)
Severity: Medium — fix
Rust: `crates/ad-plugins-rs/src/overlay.rs:219-252` spans `x..x+width` exclusive.
C: `NDPluginOverlay.cpp:120-144` `ix<=xmax` inclusive (Size+1 wide).
Impact: border one px shorter each dim, right/bottom edges inboard.

#### ADP-22: Overlay extended chars (≥128) render in Rust; C skips them (signed char)
Severity: Medium — fix
Rust: `crates/ad-plugins-rs/src/overlay.rs:82-95,309-329` renders codes 160..255.
C: `NDPluginOverlay.cpp:210-211` signed `char` makes 128..255 negative → `<32` → skipped.
Impact: non-ASCII DisplayText draws pixels in Rust, nothing in C.

#### ADP-23: Transform color layout from array attribute vs C `NDColorMode` param
Severity: Medium — verify
Rust: `crates/ad-plugins-rs/src/transform.rs:133,152,185` derives layout from the array's ColorMode attr.
C: `NDPluginTransform.cpp:527-529` reads the operator-set NDColorMode param.
Impact: when attr and record disagree, channel handling diverges. Verify reachability of the mismatch.

#### ADP-24: Process flat-field substitutes the field mean when scaleFlatField ≤ 0
Severity: Medium — fix
Rust: `crates/ad-plugins-rs/src/process.rs:369-373` uses the flat-field mean when scaleFlatField≤0.
C: `NDPluginProcess.cpp:172` `value *= scaleFlatField/flatField[i]` unconditionally (≤0 → 0).
Impact: SCALE_FLAT_FIELD≤0 — C all-zero, Rust mean-normalized.

#### ADP-25: FFT FFTTimeSeries/FFTTimeAxis posted at unpadded width; C posts the padded length
Severity: Medium — fix
Rust: `crates/ad-plugins-rs/src/fft.rs:333-335,674,686` posts at `width`.
C: `NDPluginFFT.cpp:224,245-253` posts at `nTimeX` (next-pow-2).
Impact: non-pow-2 width — waveform length/content diverge.

#### ADP-26: Codec implements zlib/lz4hdf5 codecs absent from the C reference
Severity: Medium — signoff
Rust: `crates/ad-plugins-rs/src/codec.rs:229-289,318-420`; names from `ad-core/codec.rs:9-27`.
C: `Codec.h:4-18` codec universe is `{"","jpeg","blosc","lz4","bslz4"}`.
Impact: a Rust array tagged "zlib"/"lz4hdf5" cannot be decompressed by stock C NDPluginCodec. Structural cause of ADP-11. Sign-off vs the ordinal-only fix.

#### ADP-27: netCDF missing NDNetCDFFileVersion global; writes extra uniqueId/numArrays globals
Severity: Medium — fix
Rust: `crates/ad-plugins-rs/src/file_netcdf.rs:469-495` writes uniqueId/numArrays globals, no NDNetCDFFileVersion.
C: `NDFileNetCDF.cpp:96-101` writes NDNetCDFFileVersion=3.1; uniqueId/numArrays are a variable/dimension, not globals.
Impact: a version-gating reader fails; Rust files carry two extra globals.

#### ADP-28: TIFF RGB2/RGB3 written chunky (no PlanarConfig); C writes PlanarConfig=2 separate planes
Severity: Medium — fix
Rust: `crates/ad-plugins-rs/src/file_tiff.rs:115-135,243-313` converts to interleaved RGB1, never writes PlanarConfiguration.
C: `NDFileTIFF.cpp:204-219,390-405` PLANARCONFIG_SEPARATE + planar strips.
Impact: a reader branching on PlanarConfiguration sees 2 (C) vs 1/absent (Rust).

#### ADP-29: Blosc codec params (level/shuffle/compressor) dropped from stored metadata; default clevel 3 vs 5
Severity: Low — fix-low
Rust: `crates/ad-plugins-rs/src/codec.rs:830-837` writes 0/0/0; `:790-797` default clevel 3.
C: `NDPluginCodec.cpp:399-403,894` stores real params, default clevel 5.
Impact: NTNDArray codec metadata 0/0/0 + different compressed bytes/size.

#### ADP-30: Stats HIST_BELOW/HIST_ABOVE param type Float64 vs C Int32; TIFF extra IFD tags / RowsPerStrip
Severity: Low — fix-low / signoff
Rust: `crates/ad-plugins-rs/src/stats.rs:1045-1046,1196-1197` Float64; `file_tiff.rs` (via the `tiff` crate) emits Compression/Predictor/Resolution tags + RowsPerStrip≠height.
C: `NDPluginStats.cpp:627-628,827-828` asynInt32; `NDFileTIFF.cpp:231-238` exactly 8 tags, RowsPerStrip=sizeY.
Impact: HIST value integer-equal but param type differs; TIFF IFD tag set differs (pixels identical).

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

#### SCAL-5 (RATE): `special("RATE")` posts a different field than C
Severity: Low — signoff
Rust: `crates/scaler-rs/src/records/scaler.rs:844-846` clamps RATE → framework posts RATE.
C: `scalerRecord.c:690-693` clamps `rate` but `db_post_events(&tp,...)` posts TP (apparent copy-paste bug).
Impact: a clamped RATE write posts RATE in Rust, a spurious TP in C. Replicating the C bug is not advisable. Signoff.

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

#### STD-7: time_of_day VAL uses wall clock, not the record timestamp (TSE source)
Severity: Low — signoff
Rust: `crates/std-rs/src/device_support/time_of_day.rs:48,101` use `Local::now()`/`SystemTime::now()`.
C: `devTimeOfDay.c:121,145` use `recGblGetTimeStamp` (TSE/TSEL-selected).
Impact: default TSE=0 identical; diverge only for a non-current time source. Signoff.

#### STD-8: time_of_day omits the C `<undefined>` epoch-zero sentinel
Severity: Low — signoff
Rust: `crates/std-rs/src/device_support/time_of_day.rs:56-60` always formats a date.
C: `epicsTime.cpp:176-180` writes `"<undefined>"` for secPastEpoch==0 && nsec==0.
Impact: unreachable given the wall-clock source (STD-7). Signoff.

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

#### OPT-12: QXBPM zero-quadrant-sum publishes 0.0 where C publishes NaN/±Inf
Severity: Low — verify
Rust: `crates/optics-rs/src/snl/qxbpm.rs:393-403` guards the sum and publishes 0.0.
C: `sncqxbpm.st:493-494` unguarded → ±Inf/NaN to `pos:x`/`pos:y`.
Impact: no-beam → C NaN, Rust 0.0. Verify whether the guard is intentional.

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

#### MODB-2: ASCII reader rejects lowercase hex / runt frames that C silently mis-handles
Severity: Low — signoff
Rust: `crates/modbus-rs/src/interpose.rs:92-98,204-208,239-244` errors on non-`0-9A-F` and sub-minimum frames.
C: `modbusInterpose.c:218-222,400-406` decodes lowercase to garbage / returns empty-success on runts.
Impact: no wire divergence on valid frames; Rust stricter. Signoff.

#### MODB-3: Request frame-size overflow guarded in Rust, unchecked in C
Severity: Low — signoff
Rust: `crates/modbus-rs/src/interpose.rs:151,161,172,270-276` errors above MAX_MODBUS_FRAME_SIZE=600.
C: `modbusInterpose.c:260-263` memcpy into the fixed buffer with no bound check.
Impact: no divergence for valid-size requests; Rust adds a guard. Signoff.

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
library paths). Signoff items: ADC-7, ADC-10/ADP-26 (codec vocab), SCAL-4,
SCAL-5, STD-7, STD-8, OPT-3, MQTT-3, PROC-3, MODB-1/2/3 — surfaced for the
user rather than silently changed.
