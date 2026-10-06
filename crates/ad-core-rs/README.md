# ad-core-rs

Pure Rust implementation of [EPICS areaDetector](https://github.com/areaDetector/areaDetector)
core — N-dimensional array handling and the plugin framework.

No C dependencies. Just `cargo build`.

**Repository:** <https://github.com/epics-rs/epics-rs>

## Where the rest of areaDetector lives

| Crate | Description |
|-------|-------------|
| **ad-core-rs** (this crate) | Core types: NDArray, NDArrayPool, attributes, driver/plugin base classes |
| **ad-plugins-rs** | 26 NDPlugin implementations: stats, ROI, process, transform, FFT, file I/O, etc. |
| **examples/sim-detector** | Simulated areaDetector driver (4 SimModes, Mono/RGB1, ROI crop) |

## Features

- `NDArray` — N-dimensional typed array container (the 10 C `NDDataType`s,
  Int8 through Float64)
- `NDArrayPool` — Free-list buffer reuse with memory tracking
- `NDAttributeList` — Metadata attributes through processing chain
- `NDColorMode` — Mono, Bayer, RGB1/2/3, YUV444/422
- `ADDriverBase` — Base detector driver with channel-based plugin chain
- `NDPluginProcess` trait — Pure plugin processing interface, `&self` so one
  processor can run on `NumThreads` callback threads at once
- `PluginRuntime` — Per-plugin data processing thread, with the frame
  throttler and the plugin-chain wiring
- `FileBase` / `FileController` — the capture/stream/single file lifecycle every
  file writer plugin is built on
- `simd` — the SIMD kernels the plugins' hot loops call (`simd` feature, on by
  default)

### Feature levers

| feature | what it does |
|---|---|
| `simd` (default) | the SIMD kernels, shared with `ad-plugins-rs/simd` |
| `ioc` | the iocsh/record side — `epics-base-rs`, `epics-ca-rs` and `asyn-rs/epics`, the plugin manager and the driver context |

### Plugins

The 26 plugin implementations live in `ad-plugins-rs`; its README has the
table and the notes on rayon parallelism and the shared thread pool.

### examples/sim-detector

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
cargo run -p sim-detector --features ioc --bin sim_ioc -- \
    examples/sim-detector/ioc/st.cmd
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
use ad_core_rs::driver::ad_driver::ADDriverBase;
use ad_core_rs::ndarray::{NDArray, NDDataType};

let mut driver = ADDriverBase::new("SIM1", 256, 256, 50_000_000).unwrap();
driver.connect_downstream(stats_handle.array_sender().clone());
driver.publish_array(Arc::new(array)).unwrap();
```

## Testing

```bash
cargo nextest run -p ad-core-rs
```

## Architecture

```
crates/ad-core-rs/
  src/
    ndarray.rs          # NDArray, NDDataBuffer, NDDataType
    ndarray_pool.rs     # buffer pool with memory tracking
    attributes.rs       # NDAttributeList
    color.rs            # NDColorMode
    color_layout.rs     # color-mode indexing
    pixel_cast.rs       # the PixelCast trait
    roi.rs              # ROI cropping
    codec.rs            # the codec descriptor a compressed NDArray carries
    convert.rs          # data-type conversion
    simd.rs             # the SIMD kernels (`simd` feature)
    timestamp.rs        # the NDArray timestamp pair
    finalize.rs         # end-of-acquisition finalisation
    params/             # parameter definitions
    driver/             # ADDriverBase, NDArrayDriver, ADStatus, ImageMode
    plugin/             # NDPluginProcess, PluginRuntime, channels, throttler,
                        #   wiring, FileBase/FileController
    ioc/                # iocsh side: plugin manager, driver context (`ioc`)
  db/                   # ADCore database templates
  opi/medm/             # ADCore MEDM .adl screens (66) and their .ui twins
  opi/pydm/             # PyDM .ui screens
  doc/                  # the parity notes against C ADCore

examples/sim-detector/  # the simulated driver and its sim_ioc binary
crates/ad-plugins-rs/   # the 26 plugins
```

## Requirements

- Rust 1.94.0 (`rust-toolchain.toml`), edition 2024

## License

[EPICS Open License](../../LICENSE)
