# ad-plugins-rs Compute/Processing Plugin Review vs ADCore C++

Scope: compute/processing plugins only (file writers excluded). Each Rust source
in `crates/ad-plugins-rs/src/` was compared against its `epics-modules/ADCore/
ADApp/pluginSrc/` counterpart, term-by-term for numerical code.

Severity: CRITICAL = wrong result / crash in normal use; HIGH = wrong result in
common config; MEDIUM = wrong result in non-default config or behavioral
divergence; LOW = cosmetic / edge case.

---

## Feature Gaps

### transform.rs (NDPluginTransform)
- C++ exposes 8 transform types tied to color-mode-specific stride handling
  (RGB1/RGB2/RGB3 plus Mono). Rust only handles 2-D mono buffers; the
  per-color-mode RGB1/RGB2/RGB3 reindexing in C++ `transformNDArray` is absent.
  3-D color images are returned unchanged (`src.dims.len() < 2` is the only
  guard; a 3-D array silently transforms only the first 2 dims with wrong
  layout for RGB1/RGB2). Feature gap, MEDIUM.

### overlay.rs (NDPluginOverlay)
- `OVERLAY_TIMESTAMP_FORMAT` param is registered but never read or applied. C++
  appends an `epicsTimeToStrftime`-formatted timestamp to text overlays when
  `TimeStampFormat` is non-empty. Rust text overlays cannot render timestamps.
- C++ uses real bitmap fonts (`NDPluginOverlayTextFontBitmaps`, 6 fonts of
  fixed sizes; `Font` selects the index). Rust ships a single hardcoded 5x7
  font and scales it by an arbitrary factor `(font+1)*FONT_HEIGHT`. Text
  rendering will never match C++ pixel layout.
- No `CenterX`/`CenterY` <-> `PositionX`/`PositionY` freeze semantics. C++
  `writeInt32` tracks `freezePositionX/Y` so that resizing keeps either the
  position or the center fixed depending on which was last written. Rust
  recomputes naively and has no freeze state.

### process.rs (NDPluginProcess)
- `AUTO_OFFSET_SCALE` parameter handler is a complete no-op (process.rs:737-746,
  the body only contains comments). C++ computes
  `scale = maxScale/(maxValue-minValue)`, `offset = -minValue`, enables
  offset/scale + clipping, then resets the param to 0. The Rust
  `auto_offset_scale()` method exists and is correct but is never invoked from
  the param-change path or process path. Setting `AUTO_OFFSET_SCALE` does
  nothing. HIGH.

### stats.rs (NDPluginStats)
- `HIST_X_ARRAY` waveform (the histogram bin-center X axis) param is created
  but never populated. C++ `computeHistX()` fills it on `HistSize/HistMin/
  HistMax` change. MEDIUM.
- Profile waveform params (`PROFILE_AVERAGE_X` etc.) are created but
  `process_array` never emits `ParamUpdate::float64_array` for them — only the
  scalar params and the TS sender are updated. `result.profile_*` is computed
  and stored in `latest_stats` but the profile waveforms are not pushed to
  asyn clients. MEDIUM.
- No NDArray output of the per-frame time-series array. C++ Stats emits a
  23-element `NDFloat64` NDArray every frame via `doCallbacksGenericPointer`.
  Rust routes TS through a side channel (`TimeSeriesSender`) instead — an
  architectural divergence, acceptable but not a literal port.

### attr_plot.rs (NDPluginAttrPlot)
- C++ separates `n_attributes` (tracked attributes) from `n_data_blocks`
  (waveform outputs) and maps each data block to an attribute via the
  `NDAttrPlotDataSelect` param; `NDAttrPlotDataLabel` and `NDAttrPlotNPts`
  also exist. Rust auto-detects numeric attributes from the first frame and
  maps 1:1 — `DataSelect`, `DataLabel`, `NPts` and the configured-attribute-
  name model are all missing. MEDIUM feature gap.
- C++ uses a background `ExposeDataTask` thread to push waveforms at a fixed
  period and pads unused tail with the last point. Rust pushes per-frame
  with no tail padding. Behavioral divergence, LOW.

### time_series.rs (NDPluginTimeSeries)
- The Rust `time_series.rs` is a custom channel/registry abstraction fed by
  Stats/ROIStat senders, not a literal `NDPluginProcess` operating on NDArray
  signal columns. The C++ plugin treats a 2-D array as
  `[numSignals][numTimes]`, averages `numAverage` samples per output point,
  supports Fixed vs Circular acquire modes, an elapsed-time readback and a
  signed time axis for circular mode. The averaging-by-`numAverage` and the
  per-signal column ingestion of an arbitrary input NDArray are not present
  as a generic NDArray processor. Architectural divergence; verify the params
  `ts_num_average`/`ts_acquire_mode` actually drive averaging logic.

### codec.rs (NDPluginCodec)
- C++ supports JPEG, Blosc, LZ4 and **Bitshuffle/LZ4**. Rust implements LZ4,
  JPEG and Blosc; the Bitshuffle codec is absent. MEDIUM.

### scatter.rs (NDPluginScatter)
- `num_outputs` defaults to 1 and is only set in tests — no param wires it.
  In a real IOC every array is scattered to index 0. The C++ plugin discovers
  the number of downstream consumers. Without `num_outputs` being set,
  scatter degenerates to a passthrough. MEDIUM.
- C++ scatter tries consumers in turn and advances `nextScatter`; Rust does
  pure modulo round-robin and never skips a full downstream queue. LOW.

### fft.rs (NDPluginFFT)
- C++ exposes `FFTTimeSeries`, `FFTReal`, `FFTImaginary`, `FFTAbsValue`,
  `FFTTimeAxis`, `FFTFreqAxis`, `FFTTimePerPoint` waveform/scalar params.
  Rust registers the param names but only emits the abs-value NDArray; the
  real/imaginary/time-series/axis waveforms are never pushed. MEDIUM.

### roi.rs (NDPluginROI)
- No 3-D / RGB color-mode dimension swap. C++ reorders `dims[]` for RGB1/RGB2
  so NX/NY stay the image axes. Rust `extract_roi_2d` only handles the first
  two dims; Dim2 config exists but the third dimension is never extracted.
  MEDIUM.

---

## Bugs by Severity

### CRITICAL

**circular_buff.rs:149,208 — usize underflow / panic when post_count == 0**
`CircularBuffer::trigger()` sets `post_remaining = self.post_count`. If
`post_count == 0`, the next `push()` enters the `if self.triggered` branch and
executes `self.post_remaining -= 1` on a value of 0 (circular_buff.rs:149). The
same underflow occurs on the inline-trigger path at circular_buff.rs:208 after
`self.trigger()`. In debug builds this panics; in release it wraps to
`usize::MAX`, so the buffer captures ~2^64 frames and never completes.
C++ NDPluginCircularBuff handles `postCount` via `currentPostCount >= postCount`
which is true immediately at 0. CRITICAL.

### HIGH

**bad_pixel.rs:29,150-189 — Median kernel size is half the C++ kernel**
C++ `fixBadPixelsT` loops the median kernel `for i = -medianCoordinate.y ..=
medianCoordinate.y` and `for j = -medianCoordinate.x ..= medianCoordinate.x`,
i.e. the JSON `Median:[mx,my]` value is the *half-extent*; the kernel is
`(2*mx+1) x (2*my+1)`. Rust treats `kernel_x`/`kernel_y` as the *full* kernel
size and uses `half_x = kernel_x/2` (bad_pixel.rs:152-153). For a C++ file with
`Median:[3,3]` C++ samples a 7x7 neighborhood; Rust with `kernel_x=3` samples
3x3. The median replacement value differs. HIGH.

**bad_pixel.rs:9-45,256-298 — incompatible bad-pixel JSON schema**
C++ `readBadPixelFile` parses `{"Bad pixels":[{"Pixel":[x,y],"Set":v}]}` /
`"Median":[mx,my]` / `"Replace":[dx,dy]`. Rust `BadPixelList`/`BadPixel`
deserialize `{"bad_pixels":[{"x":..,"y":..,"mode":"set","value":..}]}`. An
existing AreaDetector bad-pixel file will fail to parse in Rust and vice versa.
HIGH (data-format incompatibility).

**process.rs:737-746 — AUTO_OFFSET_SCALE does nothing** (see Feature Gaps).
The param exists, the computation helper exists, but writing the PV neither
sets a flag nor schedules `auto_offset_scale()`. HIGH.

**transform.rs:13-33 — wrong transform-type enum mapping for values 5 and 6**
C++ `NDPluginTransformType_t` order is:
`None=0, Rotate90=1, Rotate180=2, Rotate270=3, Mirror=4, Rotate90Mirror=5,
Rotate180Mirror=6, Rotate270Mirror=7`.
Rust `TransformType::from_u8` maps `4=>FlipHoriz, 5=>FlipVert, 6=>FlipDiag,
7=>FlipAntiDiag`.
- value 4 (C++ `Mirror`, horizontal flip) -> Rust `FlipHoriz` — correct.
- value 5 (C++ `Rotate90Mirror`): C++ `outData[(y*xStride)+(x*ySize)] =
  inData[(y*yStride)+(x*xStride)]` with swapped dims = transpose. Rust maps to
  `FlipVert` — WRONG, should be `FlipDiag` (transpose).
- value 6 (C++ `Rotate180Mirror`): C++ memcpy's row `y` to row `ySize-1-y` =
  vertical flip. Rust maps to `FlipDiag` — WRONG, should be `FlipVert`.
- value 7 (C++ `Rotate270Mirror`, anti-transpose): Rust `FlipAntiDiag` —
  correct by coincidence.
Selecting transform 5 or 6 from EPICS produces the wrong geometry. HIGH.

### MEDIUM

**roi.rs:121-140 — ROI offset clamp off-by-one vs C++**
C++ clamps `pDim->offset = MIN(pDim->offset, newDimSize-1)` then
`pDim->size = MAX(size,1)` and `MIN(size, newDimSize-offset)`. Rust
`extract_roi_2d` clamps `min = config.min.min(src_x)` (roi.rs:126/137) — it
allows `min == src_x` (one past the last valid index) instead of `src_x-1`.
When `min == src_x`, `size = size.min(src_x - min) = 0` and the function
returns `None` (sink), where C++ would clamp to a 1-pixel ROI at the last
column. Off-by-one divergence at the boundary. MEDIUM.

**roi.rs:177-219 — binning drops the partial last bin and never clamps bin
to size**
Rust `out_x = roi_x_size / bin_x` truncates, discarding a partial trailing
bin. C++ also truncates via the `convert()` binning, so this matches. However
C++ additionally clamps `pDim->binning = MIN(binning, (int)pDim->size)`; Rust
only does `bin.max(1)` with no upper clamp. A bin larger than the ROI yields
`out_x == 0` and a `None` result (sink); C++ would clamp bin to the ROI size
and still produce a 1-pixel output. MEDIUM.

**overlay.rs:184-205 — Cross line thickness is 2x the C++ value**
C++ `doOverlayT` Cross uses `xwide = WidthX/2`, `ywide = WidthY/2` (half-width)
and draws the central strip `xcent-xwide .. xcent+xwide` (total `2*xwide+1 ~=
WidthX` pixels). Rust uses `wx`/`wy` directly as the full thickness in the
loop `for t in 0..wy` with offset `wy_half = wy/2`. The relationship between
the `WidthX` PV and the rendered thickness differs from C++ by roughly a
factor of 2. MEDIUM.

**overlay.rs:242-265 — XOR ellipse double-toggles pixels**
C++ explicitly `std::sort` + `std::unique` the ellipse pixel offset list
"or the XOR draw mode won't work because the pixel will be set and then
unset" (NDPluginOverlay.cpp:174-178). Rust draws the ellipse by stepping
angle for each thickness layer and calls `set_pixel` directly; the same pixel
can be hit multiple times within one overlay. In `DrawMode::XOR` a
double-touched pixel is toggled back to the original value, leaving holes in
the drawn ellipse. MEDIUM.

**overlay.rs:184-205 — Cross center pixel double-XOR**
The Cross horizontal-arm loop and vertical-arm loop both write the center
pixel `(cx,cy)`. In `DrawMode::XOR` the center is toggled twice and ends up
unchanged. C++ Cross draws the center exactly once (the center row is inside
the horizontal band and the vertical strip is only added for non-band rows).
MEDIUM.

**bad_pixel.rs:139,168 — is_bad() lookup uses array coords against a
sensor-coord set**
`bad_set` is built from raw `(p.x, p.y)` sensor coordinates
(bad_pixel.rs:60,77). In `Replace`/`Median` the "is the neighbor also bad"
test calls `is_bad(nx, ny)` where `nx/ny` are *array-space* coordinates
(`adj_x + dx`). C++ constructs the neighbor in *sensor* space
(`bp.coordinate.x + replace.x*scaleX`) before the `badPixels.find` lookup. When
the detector has a non-zero readout offset or binning, the Rust bad-neighbor
check queries the wrong coordinate space and may treat a good pixel as bad or
vice versa. With default offset=0/binning=1 they coincide. MEDIUM.

**fft.rs:47-177,259-420 — no power-of-2 zero padding**
C++ NDPluginFFT rounds each dimension up to the next power of 2 (`nextPow2`),
zero-pads the time series, and computes `nFreqX = paddedX/2`. Rust runs
`rustfft` directly on the actual width (rustfft supports arbitrary sizes) and
sets `n_freq = width/2` on the *unpadded* width. For any non-power-of-2 input
the magnitude spectrum, the number of frequency bins, and the freq-axis scale
all differ from C++. (rustfft's result is arguably more correct, but it is
not C-parity.) MEDIUM.

**fft.rs:313-355,422-474 — inverse FFT outputs magnitude, losing sign/phase**
`compute_fft_1d_rows_inverse` / `compute_fft_2d_inverse` write
`c.norm() * scale` (the modulus) into the output buffer. An inverse transform
of a real-valued spectrum should yield signed real samples; taking the modulus
discards the sign. A round-trip forward->inverse of a signal with negative
samples returns all-positive values. MEDIUM.

**circular_buff.rs:345-381 — captured frames withheld until sequence
completes, not streamed**
C++ NDPluginCircularBuff, on trigger, immediately `flushPreBuffer()` (all
pre-trigger frames forwarded at once) and then forwards each post-trigger
frame individually as it arrives (`doCallbacksGenericPointer` per frame). Rust
`CircularBuffProcessor` accumulates everything in `captured` and emits the
whole batch only when `post_remaining` reaches 0. Downstream sees a burst at
the end instead of a stream; `FlushOnSoftTrig` flush-on-trigger semantics are
also not honored at the processor level. MEDIUM.

**process.rs:479-483 — frame-filter suppression forwards the input instead of
dropping**
When `filter_callbacks > 0` and the filter has not yet accumulated
`num_filter` frames, C++ sets `doCallbacks = 0` and does NOT call
`endProcessCallbacks` — the frame is dropped, nothing goes downstream. Rust
`return src.clone()` forwards the *unprocessed input array* downstream.
Downstream receives an extra (raw) frame that C++ would have suppressed.
MEDIUM.

**stats.rs:628-717 — histogram entropy normalized by full element count, not
in-range count**
This matches C++ (`entropy = -entropy / nElements`, nElements = total array
size), so it is correct. Noted here only to confirm parity — not a bug.

**roi_stat.rs:269-316 vs NDPluginROIStat.cpp:96-130 — background band
geometry differs but result is equivalent**
C++ sums the top/bottom `bgdWidthY` full rows plus the left/right `bgdWidthX`
columns of the *middle* band; Rust uses an `in_border` distance test. The two
select the same pixel set for a rectangular ROI, and both normalize
`net = total - bgd_avg * roi_elements`. Verified equivalent — not a bug.

### LOW

**stats.rs:333-618 — centroid computed directly from 2-D pixels rather than
from 1-D profiles**
C++ `doComputeCentroidT` accumulates moments from the projected `profileX`/
`profileY` arrays (a separable formulation) and only `M11` from 2-D pixels.
Rust accumulates all moments directly from 2-D pixels in a two-pass
centered-moment scheme. The two are mathematically identical for
`M00,M10,M01,M20,M02,M11` and for the derived centroid, sigma, skew, kurtosis,
eccentricity and orientation, so results agree. The threshold handling also
matches (`value >= centroidThreshold`). No numeric defect found; flagged only
because the implementation strategy diverges and any future change to one side
must preserve the equivalence.

**fft.rs:249 — freq-axis step** C++ `createAxisArrays` itself carries a
"Check this - are the frequencies correct, or off-by-one?" comment. Rust
`time_series`/`fft` freq axis uses `i * time_per_point`; minor and pre-existing
in C++. LOW.

**overlay.rs:266-288 — text inter-character gap fixed at 1px*scale** vs C++
which advances by the full font width. Cosmetic. LOW.

---

## Notes

- Plugins reviewed with no C-parity defects found in their numerical core:
  `stats.rs` statistics pass (min/max/mean/sigma/total/net, background
  subtraction matches NDPluginStats.cpp:481-530), `roi_stat.rs` ROI
  statistics, `gather.rs` (thin passthrough), `std_arrays.rs` (thin
  passthrough), `passthrough.rs`, `attribute.rs` (attribute extraction and
  sum accumulation match), `color_convert.rs` Bayer/RGB conversion (not
  exhaustively diffed against the C colorMaps tables — partial review only;
  the demosaic is a reasonable bilinear implementation but C++ NDPluginColor
  Convert uses a specific neighbor scheme that was not verified term-by-term).
- `pos_plugin.rs` parses both JSON and an XML `<position index=..>` format;
  C++ `NDPosPluginFileReader` parses an XML attribute file. The Discard/Keep
  modes are present. Not diffed in full — flagged for a dedicated pass.
- `pva.rs` exposes NDArrays over PVAccess; behavior depends on the pvxs/pva
  binding layer and was not compared term-by-term.
- `time_series.rs` is an architectural reimplementation (channel registry +
  side-channel sender) rather than a literal `NDPluginProcess`; a dedicated
  parity pass against NDPluginTimeSeries.cpp (fixed vs circular acquire mode,
  per-signal column averaging by `numAverage`, signed time axis) is
  recommended.
- The single highest-impact correctness issues to fix first:
  1. circular_buff.rs post_count==0 underflow (CRITICAL, can hang/panic).
  2. transform.rs enum mapping for values 5/6 (HIGH, wrong image geometry).
  3. bad_pixel.rs median kernel half-extent + JSON schema (HIGH, wrong
     correction values + file incompatibility).
  4. process.rs AUTO_OFFSET_SCALE no-op (HIGH, advertised feature dead).
