# Simulated Detector

A full-featured simulated areaDetector driver for epics-rs, matching the C++ [ADSimDetector](https://github.com/areaDetector/ADSimDetector) architecture.

Generates synthetic images for testing detector pipelines without real hardware.

## Simulation Modes

| Mode | Description |
|------|-------------|
| LinearRamp | Ramp from min to max across image |
| Peaks | Gaussian peaks with configurable position, count, width, height variation |
| Sine | Separable X/Y sine waves (2 components per axis, Add or Multiply) |
| OffsetNoise | Noise floor with configurable offset |

## Architecture

```
SimDetector (PortDriver)
    │
    ├── params.rs        35+ asyn parameters (gains, offsets, modes)
    ├── compute.rs       Pixel-level image generation per mode
    ├── task.rs          Acquisition thread (continuous frame generation)
    ├── types.rs         SimMode, SineOperation, DirtyFlags
    └── ioc_support.rs   Device support bridge, 150+ PV mappings
```

The driver uses a dirty flag system to avoid recomputing caches unnecessarily. Changing a gain parameter only invalidates the gain cache, not the peak positions.

## Acquisition Task API

Acquisition tasks are fully async. A driver author only needs three types from `ad_core_rs::plugin::channel`:

| Type | Purpose |
|------|---------|
| `PortHandle` | Read/write parameters via `read_int32().await`, `set_params_and_notify().await` |
| `ArrayPublisher` | Publish a generated frame to downstream plugins: `publisher.publish(frame).await` |
| `QueuedArrayCounter` | Wait until in-flight frames drain at end of acquisition |

The acquisition loop runs inside a `current_thread` tokio runtime created by the task thread. All I/O is async and reliable — there are no lossy or blocking APIs in the data path.

```rust
async fn acquisition_loop_async(mut ctx: AcquisitionContext) {
    loop {
        // Read config via PortHandle (async)
        let acquire_time = ctx.port_handle.read_float64(ctx.ad.acquire_time, 0).await?;

        // Generate frame...

        // Batch-update params via PortHandle (async, reliable)
        ctx.port_handle.set_params_and_notify(0, vec![...]).await?;

        // Publish frame to downstream plugins (concurrent fan-out)
        if config.array_callbacks {
            ctx.publisher.publish(Arc::new(frame)).await;
        }
    }
}
```

## Parameters

### Gains

| Parameter | Type | Description |
|-----------|------|-------------|
| `SIM_GAIN_X`, `SIM_GAIN_Y` | Float64 | Per-axis gain multipliers |
| `SIM_GAIN_RED/GREEN/BLUE` | Float64 | Per-color gains (RGB mode) |

### Peak Mode

| Parameter | Type | Description |
|-----------|------|-------------|
| `SIM_PEAK_START_X/Y` | Int32 | Initial peak position |
| `SIM_PEAK_NUM_X/Y` | Int32 | Number of peaks per dimension |
| `SIM_PEAK_STEP_X/Y` | Int32 | Spacing between peaks |
| `SIM_PEAK_WIDTH_X/Y` | Int32 | Peak width (sigma) |
| `SIM_PEAK_HEIGHT_VARIATION` | Float64 | Random height variation |

### Sine Mode

| Parameter | Type | Description |
|-----------|------|-------------|
| `SIM_XSINE1/2_AMPLITUDE` | Float64 | X sine component amplitudes |
| `SIM_XSINE1/2_FREQUENCY` | Float64 | X sine component frequencies |
| `SIM_XSINE1/2_PHASE` | Float64 | X sine component phases |
| `SIM_YSINE1/2_*` | Float64 | Y sine equivalents |
| `SIM_XSINE_OPERATION` | Int32 | Add (0) or Multiply (1) |

### General

| Parameter | Type | Description |
|-----------|------|-------------|
| `SIM_MODE` | Int32 | Simulation mode (0-3) |
| `SIM_OFFSET` | Float64 | DC offset added to all pixels |
| `SIM_NOISE` | Float64 | Random noise amplitude |
| `RESET_IMAGE` | Int32 | Trigger buffer reallocation |

## Build and Run

```bash
# Library only
cargo build -p sim-detector --features ioc

# Full IOC with plugins
cargo build --release -p sim-detector --features ioc --bin sim_ioc

# Run IOC
./target/release/sim_ioc examples/sim-detector/ioc/st.cmd

# Standalone demo (no EPICS IOC, just PortHandle API)
cargo run -p sim-detector --example demo
```

## st.cmd Configuration

```bash
epicsEnvSet("PREFIX", "SIM1:")
epicsEnvSet("CAM",    "cam1:")

# simDetectorConfig(portName, sizeX, sizeY, maxMemory)
simDetectorConfig("SIM1", 1024, 1024, 50000000)

dbLoadRecords("$(SIM_DETECTOR)/db/simDetector.template", "P=$(PREFIX),R=$(CAM),PORT=SIM1")

# Standard areaDetector plugins
< $(ADCORE)/ioc/commonPlugins.cmd
```

## Autosave

The sim-detector IOC includes full autosave support, matching the C EPICS autosave workflow. PV values are automatically saved to disk and restored on IOC restart.

### Configuration in st.cmd

```bash
# Search paths for .req files (detector, plugins)
set_requestfile_path("$(ADSIMDETECTOR)/db")
set_requestfile_path("$(ADCORE)/db")

# Directory for .sav files
set_savefile_path("$(ADSIMDETECTOR)/ioc/autosave")

# Status PV prefix
save_restoreSet_status_prefix("$(PREFIX)")

# Restore saved values (pass0 = before device init, pass1 = after)
set_pass0_restoreFile("simDetector_settings.req", "P=$(PREFIX),R=$(CAM)")
set_pass1_restoreFile("simDetector_settings.req", "P=$(PREFIX),R=$(CAM)")

# Periodic save every 5 seconds
create_monitor_set("simDetector_settings.req", 5, "P=$(PREFIX),R=$(CAM)")
```

### Request File Structure

`simDetector_settings.req` defines which PVs to save. It uses `file` includes to pull in settings from ADBase and all common plugins:

```
$(P)$(R)GainX
$(P)$(R)SimMode
...
file "ADBase_settings.req", P=$(P), R=$(R)
file "commonPlugins_settings.req", P=$(P)
```

`commonPlugins_settings.req` includes all standard areaDetector plugin settings (Stats, ROI, FFT, file writers, overlays, etc.), which in turn include their own nested `.req` files. Macros like `$(P)` are expanded through the include chain.

### Runtime Commands

Once the IOC is running, use these iocsh commands:

```
fdblist                    # List all save sets and their status
fdbsave simDetector_settings.req   # Trigger immediate save
fdbrestore simDetector_settings    # Restore from .sav file
```

## Verify

```bash
# Set simulation mode to Peaks
caput SIM1:cam1:SimMode 1

# Acquire single image
caput SIM1:cam1:ImageMode 0
caput SIM1:cam1:Acquire 1

# Monitor stats
camonitor SIM1:Stats1:MeanValue_RBV
```

## AD Viewer (PyDM)

A PyDM display at `opi/pydm/adViewer.py` shows the CA (`image1:ArrayData`)
and PVA (`Pva1:Image`) plugin outputs side by side, with Start/Stop
controls and per-plugin Enable toggles.

```bash
pydm examples/sim-detector/opi/pydm/adViewer.py \
    --macro "P=SIM1:,R=cam1:,IMAGE=image1:,PVA=Pva1:,PVA_PV=SIM1:Pva1:Image"
```

Macros:

| Macro   | Default              | Meaning                      |
|---------|----------------------|------------------------------|
| `P`     | `SIM1:`              | Shared prefix                |
| `R`     | `cam1:`              | Camera record suffix         |
| `IMAGE` | `image1:`            | CA plugin suffix             |
| `PVA`   | `Pva1:`              | PVA plugin suffix            |
| `PVA_PV`| `SIM1:Pva1:Image`    | Name under which PVA serves  |

## License

MIT
