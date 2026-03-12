# areadetector-rs

Pure Rust implementation of [EPICS areaDetector](https://github.com/areaDetector/areaDetector) — N-dimensional array handling, plugin framework, and simulated detector driver.

No C dependencies. Just `cargo build`.

## Workspace

| Crate | Description |
|-------|-------------|
| **ad-core** | Core types: NDArray, NDArrayPool, attributes, driver/plugin base classes |
| **plugins** | 16 NDPlugin implementations: stats, ROI, process, transform, FFT, file I/O, etc. |
| **sim-detector** | Simulated areaDetector driver (4 SimModes, Mono/RGB1, ROI crop) |

## Features

### ad-core

- `NDArray` — N-dimensional typed array container (10 data types)
- `NDArrayPool` — Free-list buffer reuse with memory tracking
- `NDAttributeList` — Metadata attributes through processing chain
- `NDColorMode` — Mono, Bayer, RGB1/2/3, YUV444/422
- `ADDriverBase` — Base detector driver with channel-based plugin chain
- `NDPluginProcess` trait — Pure plugin processing interface
- `PluginRuntime` — Per-plugin data processing thread

### plugins

| Plugin | Description |
|--------|-------------|
| stats | Min/max/mean/sigma, centroid |
| roi | Region of interest with binning |
| process | Arithmetic, morphology, filters |
| transform | Flip, rotate, transpose |
| color_convert | Bayer/RGB/YUV conversions |
| overlay | Draw shapes on images |
| fft | Fast Fourier Transform |
| time_series | Temporal data ringbuffer |
| circular_buff | Array history buffer |
| codec | JPEG/PNG/Zlib compression |
| gather | Combine arrays |
| scatter | Distribute arrays |
| std_arrays | Standard array generation |
| file_tiff | TIFF file writing |
| file_jpeg | JPEG file writing |
| file_hdf5 | HDF5 file writing |

### Parallel Processing

The `parallel` feature (enabled by default in ad-plugins) uses rayon to parallelize CPU-heavy plugins (Stats, ROIStat, ColorConvert, Process). A shared thread pool sized to `available_cores - 2` prevents over-subscription with port driver threads. See the [ad-plugins README](../ad-plugins/README.md#parallel-processing) for details.

### sim-detector

- 4 simulation modes: LinearRamp, Peaks, Sine, OffsetNoise
- Color modes: Mono, RGB1
- ROI cropping with min_x/y, size_x/y
- Actor-based acquisition with PortHandle I/O and channel-based start/stop
- Single and Continuous image modes
- Configurable gains, noise, peak parameters
- IOC support with st.cmd (`ioc` feature)

## Quick Start

### Run SimDetector IOC

```bash
cargo run -p sim-detector --features ioc --bin sim_ioc -- ioc/st.cmd
```

### st.cmd

```bash
epicsEnvSet("PREFIX", "SIM1:")
epicsEnvSet("CAM",    "cam1:")
epicsEnvSet("EPICS_DB_INCLUDE_PATH", "$(ADCORE)/ADApp/Db")
simDetectorConfig("SIM1", 256, 256, 50000000)
dbLoadRecords("$(ADSIMDETECTOR)/simDetectorApp/Db/simDetector.template", "P=$(PREFIX),R=$(CAM),PORT=SIM1,DTYP=asynSimDetector")
iocInit()
```

### Library Usage

```rust
use ad_core::ndarray::{NDArray, NDDataType};
use ad_core::driver::ad_driver::ADDriverBase;

let mut driver = ADDriverBase::new("SIM1", 256, 256, 50_000_000).unwrap();
driver.connect_downstream(stats_handle.array_sender().clone());
driver.publish_array(Arc::new(array)).unwrap();
```

## Testing

```bash
cargo test --workspace
```

90 tests (38 ad-core + 8 plugins + 41 sim-detector unit + 3 integration).

## Architecture

```
areadetector-rs/
  ad-core/
    src/
      ndarray.rs          # NDArray, NDDataBuffer, NDDataType
      ndarray_pool.rs     # Buffer pool
      attributes.rs       # NDAttributeList
      color.rs            # NDColorMode
      params/             # Parameter definitions
      driver/             # ADDriverBase, ADStatus, ImageMode
      plugin/             # NDPluginProcess, PluginRuntime, channels
    opi/
      medm/               # ADCore MEDM .adl screens (66)
      pydm/               # PyDM .ui screens
  plugins/
    src/
      stats.rs .. file_hdf5.rs  # 16 plugin implementations
  sim-detector/
    src/
      types.rs            # SimMode, DirtyFlags
      pixel_cast.rs       # PixelCast trait
      color_layout.rs     # Color mode indexing
      compute.rs          # Image generation (4 modes)
      roi.rs              # ROI cropping
      driver.rs           # SimDetector + PortDriver impl
      task.rs             # Acquisition thread
    Db/                   # Database templates
    ioc/                  # st.cmd startup scripts
    opi/
      medm/               # SimDetector MEDM .adl screens
      pydm/               # PyDM .ui screens
```

## Requirements

- Rust 1.70+
- tokio runtime

## License

The Rust code authored in this crate is licensed under MIT.

This crate also bundles third-party OPI/UI assets from the areaDetector project
family. See [`THIRD_PARTY_LICENSES`](THIRD_PARTY_LICENSES) for attribution and
upstream license text.
