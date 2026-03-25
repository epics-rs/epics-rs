# ad-plugins

NDPlugin implementations for [areaDetector-rs](../ad-core/). 23 image processing and data handling plugins for real-time detector data pipelines.

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
| **NDFileHDF5** | HDF5 file writer (feature-gated `hdf5` crate, with binary fallback) |
| **NDFileJPEG** | JPEG encoding/decoding via jpeg-encoder/jpeg-decoder |
| **NDFileTIFF** | TIFF encoding/decoding via tiff crate |

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

## Features

```toml
[features]
default = ["parallel"]
parallel = ["rayon"]    # Rayon data-parallelism for CPU-heavy plugins
hdf5 = ["dep:hdf5-metno"]  # HDF5 file format (built from bundled source, requires cmake)
ioc  = ["ad-core/ioc"]  # IOC startup commands (NDStatsConfigure, etc.)
```

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

All plugins share a single rayon `ThreadPool` to avoid over-subscription when multiple plugins process data concurrently. The pool is sized to `available_cores - 2` (minimum 1), reserving headroom for port driver data threads, autoconnect tasks, and the tokio runtime.

To override the thread count, call `set_num_threads()` before the first array is processed:

```rust
ad_plugins::par_util::set_num_threads(4);
```

A minimum element threshold (`PAR_THRESHOLD = 4096`) prevents rayon overhead from dominating on small arrays. Below this threshold, the sequential path is used automatically.

To disable parallelism entirely:

```toml
ad-plugins = { version = "0.2", default-features = false }
```

## Usage

Each plugin implements `NDPluginProcess`:

```rust
use ad_plugins::stats::NDStatsPlugin;
use ad_core::plugin::NDPluginProcess;

let mut stats = NDStatsPlugin::new();
let result = stats.process_array(&array, &pool);
// result.arrays contains processed output
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
cargo build -p ad-plugins                    # plugins only
cargo build -p ad-plugins --features ioc     # with IOC commands
cargo build -p ad-plugins --features hdf5    # with HDF5 support
cargo test -p ad-plugins                     # 205 tests
```

## License

MIT
