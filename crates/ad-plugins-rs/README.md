# ad-plugins-rs

NDPlugin implementations for [ad-core-rs](../ad-core-rs/). 26 image processing
and data handling plugins for real-time detector data pipelines.

No C dependencies. Pure Rust with real encoding libraries (JPEG, TIFF, LZ4, FFT).

**Repository:** <https://github.com/epics-rs/epics-rs>

## Plugins

### Statistical Analysis

| Plugin | Description |
|--------|-------------|
| **NDStats** | Min/max/mean/sigma/total, centroid with threshold, histogram, row/column profiles, higher-order moments |
| **NDROIStat** | Multi-ROI statistics with background subtraction and time series |
| **NDAttrPlot** | Circular buffer tracking of numeric NDArray attributes |
| **NDAttribute** | Single attribute extraction with cumulative sum |

### Image Transformation

| Plugin | Description |
|--------|-------------|
| **NDROIPlugin** | Region-of-interest extraction with auto-center (CoM/Peak), binning, per-dimension enable |
| **NDTransform** | 8 geometric transforms: rotations (90/180/270), flips (H/V/diagonal) |
| **NDColorConvert** | RGB/Mono conversion, Bayer demosaic (bilinear), false-color jet LUT |
| **NDOverlay** | Draw shapes (cross, rectangle, ellipse, text) with 5x7 bitmap font |

### Array Processing

| Plugin | Description |
|--------|-------------|
| **NDProcess** | 4-tap recursive filter, background/flatfield save, offset/scale, clipping |
| **NDFFT** | 1D (rows) and 2D (separable) FFT via rustfft, DC suppression, frame averaging |

### Data Buffering

| Plugin | Description |
|--------|-------------|
| **NDCircularBuff** | Pre/post-trigger ring buffer with Calc expression evaluator |
| **NDTimeSeries** | OneShot/RingBuffer accumulation for statistics channels |
| **NDStdArrays** | Latest array storage (passthrough with caching) |

### File I/O

| Plugin | Description |
|--------|-------------|
| **NDFileHDF5** | HDF5 file writer via `rust-hdf5`, with the custom-layout XML engine of C's `NDFileHDF5` |
| **NDFileJPEG** | JPEG encoding/decoding via jpeg-encoder/jpeg-decoder |
| **NDFileTIFF** | TIFF encoding/decoding via the `image` crate |
| **NDFileNetCDF** | netCDF-3 classic writer |
| **NDFileNexus** | NeXus file writer |
| **NDFileMagick** | the ImageMagick-style format set, via the `image` crate |

### Codec

| Plugin | Description |
|--------|-------------|
| **NDCodec** | Lossless compression: LZ4 (lz4_flex) + JPEG, preserves original data type |

### Position & Pixel Correction

| Plugin | Description |
|--------|-------------|
| **NDPos** | Attach position metadata from JSON list (Discard/Keep modes) |
| **NDBadPixel** | Bad pixel correction: Set (fixed value), Replace (neighbor), Median (kernel). JSON config |

### Multiplexing

| Plugin | Description |
|--------|-------------|
| **NDGather** | Passthrough multiplexer (many → one) |
| **NDScatter** | Round-robin splitter (one → many) |
| **Passthrough** | No-op stub for unimplemented plugin types |

### Serving

| Plugin | Description |
|--------|-------------|
| **NDPluginPva** | serves the latest NDArray as an NTNDArray over pvAccess (`pva` feature) |

## Features

```toml
[features]
default   = ["parallel", "simd"]
parallel  = ["rayon"]       # rayon data-parallelism for CPU-heavy plugins
simd      = ["fearless_simd", "ad-core-rs/simd"]  # the SIMD kernels
ioc       = ["ad-core-rs/ioc"]  # IOC startup commands (NDStatsConfigure, etc.)
pva       = ["epics-pva-rs", "epics-bridge-rs"]   # NDPluginPva
```

HDF5 is not feature-gated: `rust-hdf5` is an unconditional dependency, built
with `threadsafe`, `all_filters` and `parallel`.

### Parallel Processing

The `parallel` feature (enabled by default) uses [rayon](https://docs.rs/rayon) to parallelize CPU-heavy image processing in 4 plugins:

| Plugin | Parallelized Operations |
|--------|------------------------|
| **NDROIStat** | Per-ROI stats computation (`par_iter` over ROI regions) |
| **NDStats** | Basic stats (fold+reduce), centroid (fold+reduce), histogram (par_chunks + merge) |
| **NDColorConvert** | Bayer demosaic (`bayer_to_rgb1` row-parallel) |
| **NDProcess** | Stages 1–4: background, flat field, offset/scale, clipping (element-wise `par_iter_mut`) |

Not parallelized: FFT (rustfft internal SIMD), recursive filter (IIR dependency chain), profiles (memory access pattern), Overlay/Transform (lightweight).

**Thread pool management:**

All plugins share a single rayon `ThreadPool` to avoid over-subscription when multiple plugins process data concurrently. The pool is sized to `available_cores - 2` (minimum 1), reserving headroom for port driver data threads, autoconnect tasks, and the async runtime.

To override the thread count, call `set_num_threads()` before the first array is processed:

```rust
ad_plugins_rs::par_util::set_num_threads(4);
```

A minimum element threshold (`PAR_THRESHOLD = 4096`) prevents rayon overhead from dominating on small arrays. Below this threshold, the sequential path is used automatically.

To disable parallelism entirely:

```toml
ad-plugins-rs = { version = "0.30", default-features = false }
```

## Usage

Each plugin implements `NDPluginProcess`:

```rust
use ad_core_rs::plugin::NDPluginProcess;
use ad_plugins_rs::stats::StatsProcessor;

let stats = StatsProcessor::new();
// `&self`, not `&mut self`: the runtime runs one processor on NumThreads
// callback threads at once, so a stateful plugin owns its own locking.
let result = stats.process_array(&array, &pool);
// result.output_arrays carries the processed frames, result.param_updates
// the parameter writes the runtime posts.
```

With the `ioc` feature, plugins register as st.cmd startup commands:

```bash
# In st.cmd
NDStatsConfigure("STATS1", 5, "SIM1")
NDROIConfigure("ROI1", 5, "SIM1")
NDStdArraysConfigure("IMAGE1", 5, "SIM1")
```

## Build

```bash
cargo build -p ad-plugins-rs                        # plugins only
cargo build -p ad-plugins-rs --features ioc,pva     # with IOC commands and NDPluginPva
cargo nextest run -p ad-plugins-rs
```

## License

[EPICS Open License](../../LICENSE)
