# epics-rs

Pure Rust implementation of the [EPICS](https://epics-controls.org/) control system framework.

No C dependencies. No `libca`. No `libCom`. Just `cargo build`.

**100% wire-compatible** with C EPICS clients (`caget`, `camonitor`, CSS, etc.).

## Motivation

EPICS is the proven standard for large-scale control systems at accelerator facilities, synchrotron light sources, fusion experiments, and beyond. Its ecosystem of support modules — asyn, motor, areaDetector, calc, sequencer, autosave, and many more — represents decades of field-tested engineering.

As a controls engineer working across many device types, I needed an environment where **every device could be simulated in software** — motors, detectors, beam diagnostics — all running together on a single laptop without any real hardware. EPICS already supports this through simulation drivers, but the path to get there involves building EPICS Base, then each support module in dependency order, configuring `RELEASE` paths between them, writing `.dbd` registrations, and wiring `Makefile` rules. For experienced EPICS developers this is routine work, but it adds up when the goal is simply to prototype a new driver or test a control sequence.

To give a concrete example: the sim-detector IOC in this project boots with **7,387 records** (5,273 with device support, 1,543 I/O Intr scanned). Reaching that scale in C EPICS means building and linking EPICS Base, asyn, areaDetector core, and every plugin (Stats, ROI, FFT, file writers, overlay, etc.) — each with its own `configure/RELEASE`, `Makefile`, and `.dbd` wiring. In epics-rs, the same full-featured areaDetector plugin environment is a single `cargo build`.

epics-rs takes a different approach to this setup problem by leveraging Rust's Cargo package system. All support modules live in a single workspace, dependencies are declared in `Cargo.toml`, and the entire stack — from Channel Access protocol to areaDetector plugins — builds with one command:

```bash
cargo build --release --workspace
```

This makes it straightforward to spin up a new device simulator:

```bash
cargo new my-device-sim
cd my-device-sim
# Add epics-rs dependencies to Cargo.toml
cargo run --release
```

The wire protocol is identical to C EPICS, so existing clients (`caget`, `camonitor`, CSS, PyDM, Phoebus) work without modification. The goal is not to replace C EPICS in production facilities, but to provide a **fast path from idea to running simulation** — where the focus stays on device logic rather than build infrastructure.

## Overview

epics-rs reimplements the core components of C/C++ EPICS in Rust:

- **Channel Access protocol** — client & server (UDP name resolution + TCP virtual circuit)
- **IOC runtime** — 20 record types, .db file loading, link chains, scan scheduling
- **asyn framework** — actor-based async port driver model
- **Motor record** — 9-phase state machine, coordinate transforms, backlash compensation
- **areaDetector** — NDArray, driver base, 23 plugins
- **Sequencer** — SNL compiler + runtime
- **Calc engine** — numeric/string/array expressions
- **Autosave** — PV save/restore
- **msi** — macro substitution & include tool

## Workspace Structure

```
epics-rs/
├── crates/
│   ├── epics-base/       # CA protocol, IOC runtime, 20 record types, iocsh
│   ├── epics-macros/     # #[derive(EpicsRecord)] proc macro
│   ├── asyn/             # Async device I/O framework (port driver model)
│   ├── motor/            # Motor record + SimMotor
│   ├── ad-core/          # areaDetector core (NDArray, NDArrayPool, driver base)
│   ├── ad-plugins/       # 23 NDPlugins (Stats, ROI, FFT, TIFF, JPEG, HDF5, etc.)
│   ├── calc/             # Calc expression engine (numeric, string, array, math)
│   ├── seq/              # Sequencer runtime (state machine execution)
│   ├── snc-core/         # SNL compiler library (lexer, parser, codegen)
│   ├── snc/              # SNL compiler CLI
│   ├── autosave/         # PV automatic save/restore
│   ├── busy/             # Busy record
│   └── msi/              # Macro substitution & include tool (.template → .db)
└── examples/
    ├── scope-ioc/        # Digital oscilloscope simulator
    ├── mini-beamline/    # Beamline simulator with 5 motors + detectors
    ├── sim-detector/     # areaDetector simulation driver
    └── seq-demo/         # Sequencer demo
```

### Crate Dependency Graph

```
epics-base-rs ◄─── epics-macros (proc macro)
    ▲
    ├── calc-rs (epics feature)
    ├── autosave-rs
    ├── busy-rs
    ├── seq
    │    └── snc-core
    ├── asyn-rs (epics feature)
    │    └── motor-rs
    └── ad-core (ioc feature)
         ├── asyn-rs
         └── ad-plugins
              └── asyn-rs

msi-rs (standalone — no EPICS dependency)
```

## Architecture: C EPICS vs epics-rs

### Key Design Differences

| Aspect | C EPICS | epics-rs |
|--------|---------|----------|
| **Concurrency model** | POSIX threads + mutex pool + event queue | tokio async + per-driver actor (exclusive ownership) |
| **Record internals** | C struct fields, `dbAddr` pointer arithmetic | Rust trait system, on-demand `Snapshot` assembly |
| **Device drivers** | C functions + `void*` pointers | Rust traits + impl blocks (type-safe) |
| **Metadata storage** | Stored directly in record C struct (flat memory) | Assembled on-demand into `Snapshot` (Display/Control/EnumInfo) |
| **Module system** | `.dbd` files + `Makefile` | Cargo workspace + feature flags |
| **Link resolution** | `dbAddr` pointer offsets | Trait methods + field name dispatch |
| **Memory safety** | Manual management (segfault possible) | Safe Rust (no unsafe in record logic) |
| **IOC configuration** | `st.cmd` shell script | Rust builder API or `st.cmd`-compatible parser |
| **Wire format** | CA protocol | **Identical** (fully compatible with C clients/servers) |

### 1. Actor-Based Concurrency

C EPICS uses a global shared state with mutex pools. In epics-rs, each driver has a tokio actor with exclusive ownership — no `Arc<Mutex>` on the hot path:

```
C EPICS:                          epics-rs:
┌──────────────────┐              ┌──────────────────┐
│  Global State    │              │   PortActor      │ ← exclusive ownership
│  + Mutex Pool    │              │   (tokio task)   │
│  + Event Queue   │              ├──────────────────┤
│                  │              │   PortHandle     │ ← cloneable interface
│  Thread 1 ──lock─┤              │   (mpsc channel) │
│  Thread 2 ──lock─┤              └──────────────────┘
│  Thread 3 ──lock─┤
└──────────────────┘
```

### 2. Snapshot-Based Metadata Model

C EPICS reads GR/CTRL data directly from the record struct's memory. In epics-rs, the `Snapshot` type bundles value + alarm + timestamp + metadata together:

```
┌──────────────────────────────────────────────────────┐
│                     Snapshot                          │
│  value: EpicsValue                                    │
│  alarm: AlarmInfo { status, severity }                │
│  timestamp: SystemTime                                │
│  display: Option<DisplayInfo>  ← EGU, PREC, HOPR/LOPR│
│  control: Option<ControlInfo>  ← DRVH/DRVL            │
│  enums:   Option<EnumInfo>     ← ZNAM/ONAM, ZRST..FFST│
└──────────────────────────────────────────────────────┘
        │
        ▼  encode_dbr(dbr_type, &snapshot)
┌──────────────────────────────────────────────────────┐
│  DBR_PLAIN (0-6)   → bare value                      │
│  DBR_STS   (7-13)  → status + severity + value       │
│  DBR_TIME  (14-20) → status + severity + stamp + val │
│  DBR_GR    (21-27) → sts + units + prec + limits + v │
│  DBR_CTRL  (28-34) → sts + units + prec + ctrl + val │
└──────────────────────────────────────────────────────┘
```

### 3. Pure Data Protocol Types

Instead of C EPICS's callback chains, epics-rs uses serializable message types:

```rust
// No trait objects or closures — pure data
enum PortCommand {      // 23 variants
    ReadInt32 { addr, reason },
    WriteFloat64 { addr, reason, value },
    ReadOctetArray { addr, reason, max_len },
    // ...
}
enum PortReply { ... }
enum PortEvent { ... }
```

This enables future wire transport extensions (Unix sockets, network) and simplifies testing.

### 4. Module System: `.dbd` → Cargo

| C EPICS | epics-rs |
|---------|----------|
| `.dbd` files (module declarations) | `Cargo.toml` `[dependencies]` |
| `Makefile` `xxx_DBD +=` | Add/remove crate dependencies |
| `envPaths` (build-time path generation) | `DB_DIR` const via `CARGO_MANIFEST_DIR` |
| `registrar()` / `device()` in `.dbd` | `register_device_support()` call |
| `#ifdef` conditional include | Cargo `features` |

### 5. Record System Separation

In C EPICS, each record type requires separate `.dbd` and C source files. epics-rs splits the record system into two layers:

- **`record.rs`** — shared infrastructure for all record types (`CommonFields`, `Record` trait, `RecordInstance`, link parsing, field get/put, alarm logic)
- **`records/*.rs`** — per-record-type files. `#[derive(EpicsRecord)]` generates boilerplate

Adding a new record type requires only a new file in `records/` — no changes to `record.rs`.

## Record Types

| Type | Description | Value Type |
|------|-------------|------------|
| ai | Analog input | Double |
| ao | Analog output | Double |
| bi | Binary input | Enum (u16) |
| bo | Binary output | Enum (u16) |
| longin | Long input | Long (i32) |
| longout | Long output | Long (i32) |
| mbbi | Multi-bit binary input | Enum (u16) |
| mbbo | Multi-bit binary output | Enum (u16) |
| stringin | String input | String |
| stringout | String output | String |
| waveform | Array data | DoubleArray / LongArray / CharArray |
| calc | Calculation | Double |
| calcout | Calculation with output | Double |
| fanout | Forward link fanout | — |
| dfanout | Data fanout | Double |
| seq | Sequence | Double |
| sel | Select | Double |
| compress | Circular buffer / N-to-1 compression | DoubleArray |
| histogram | Signal histogram | LongArray |
| sub | Subroutine | Double |

## Quick Start

### Build

```bash
cargo build --workspace
```

### Run a Soft IOC

```bash
# Simple PVs
softioc-rs --pv TEMP:double:25.0 --pv MSG:string:hello

# Record-based
softioc-rs --record ai:SENSOR:0.0 --record bo:SWITCH:0

# From a .db file
softioc-rs --db my_ioc.db -m "P=TEST:,R=TEMP"
```

### CA Client Tools

```bash
caget-rs TEMP              # read
caput-rs TEMP 42.0          # write
camonitor-rs TEMP           # subscribe
cainfo-rs TEMP              # metadata
```

C EPICS clients (`caget`, `camonitor`, CSS, PyDM, etc.) also work as-is.

### Library Usage

#### Declarative IOC Builder

```rust
use epics_base_rs::server::ioc_app::IocApplication;
use epics_base_rs::server::records::ao::AoRecord;
use epics_base_rs::server::records::bi::BiRecord;

IocApplication::new()
    .record("TEMP", AoRecord::new(25.0))
    .record("INTERLOCK", BiRecord::new(0))
    .run()
    .await?;
```

#### IocApplication (st.cmd Style)

```rust
use epics_base_rs::server::ioc_app::IocApplication;

IocApplication::new()
    .register_device_support("myDriver", || Box::new(MyDeviceSupport::new()))
    .startup_script("ioc/st.cmd")
    .run()
    .await?;
```

st.cmd uses **the same syntax as C++ EPICS**:

```bash
epicsEnvSet("PREFIX", "SIM1:")
myDriverConfig("SIM1", 256, 256, 50000000)
dbLoadRecords("$(MY_DRIVER)/Db/myDriver.db", "P=$(PREFIX)")
iocInit()
```

#### CA Client Library

```rust
use epics_base_rs::client::CaClient;

let client = CaClient::new().await?;
let (_type, value) = client.caget("TEMP").await?;
client.caput("TEMP", "42.0").await?;
```

## Crate Details

### epics-base-rs

CA protocol client/server, IOC runtime, 20 record types, iocsh, access security, autosave integration.

- UDP name resolution + TCP virtual circuit
- Extended CA header (>64 KB payloads)
- Beacon emitter, monitor subscriptions
- ACF file parser (UAG/HAG/ASG rules)
- pvAccess client (experimental)

### asyn-rs

Rust port of C EPICS asyn. Actor-based port driver model:

- **PortDriver trait** — `read_int32`, `write_float64`, `read_octet_array`, etc.
- **ParamList** — change tracking, timestamps, alarm propagation
- **PortActor** — exclusive driver ownership (tokio task)
- **PortHandle** — cloneable async interface
- **RuntimeClient** — transport abstraction (InProcessClient, future UnixSocketClient)

### motor-rs

Complete motor record implementation:

- **9-phase motion state machine** — Idle, MainMove, BacklashApproach, BacklashFinal, Retry, Jog, JogStopping, JogBacklash, Homing
- **Coordinate transforms** — User <-> Dial <-> Raw (steps)
- **Backlash compensation** — approach + final move
- **4 retry modes** — Default, Arithmetic, Geometric, InPosition
- **AxisRuntime** — per-axis tokio actor, poll loop
- **SimMotor** — time-based linear interpolation motor for testing

### ad-core & ad-plugins

areaDetector framework:

- **NDArray** — N-dimensional typed array (10 data types)
- **NDArrayPool** — free-list buffer reuse
- **ADDriverBase** — detector driver base (Single/Multiple/Continuous modes)
- **23 plugins** — Stats, ROI, ROIStat, Process, Transform, ColorConvert, Overlay, FFT, TimeSeries, CircularBuff, Codec, Gather, Scatter, StdArrays, FileTIFF, FileJPEG, FileHDF5, Attribute, AttrPlot, BadPixel, PosPlugin, Passthrough
- **Parallel processing** — rayon data-parallelism for CPU-heavy plugins (Stats, ROIStat, ColorConvert, Process). Shared thread pool sized to `available_cores - 2` to leave headroom for driver threads and tokio runtime. Enabled by default; see [ad-plugins README](crates/ad-plugins/README.md#parallel-processing)

### calc-rs

Expression engine:

- **Numeric** — infix-to-postfix compilation, 16 input variables (A-P), math functions
- **String** — string manipulation, 12 string variables (AA-LL)
- **Array** — element-wise operations, statistics (mean, sigma, min, max, median)
- **EPICS records** — transform, scalcout, sseq (epics feature)

### seq & snc-core

EPICS sequencer:

- **Runtime (seq)** — state set execution, pvGet/pvPut/pvMonitor, event flags
- **Compiler (snc-core)** — SNL lexer/parser, AST, IR, semantic analysis, Rust code generation

### autosave-rs

PV automatic save/restore:

- Periodic/triggered/on-change/manual save strategies
- Atomic file write (tmp -> fsync -> rename)
- Backup rotation (`.savB`, sequence files, dated backups)
- C autosave-compatible format

### msi-rs

Macro substitution & include tool:

- `.template` -> `.db` conversion
- `$(KEY)`, `${KEY}`, `$(KEY=default)`, `$$` escape
- C EPICS msi-compatible output

## Running the Examples

All examples are self-contained IOCs that simulate real hardware. Each one builds from source with no external dependencies beyond Rust and Cargo.

> **Always use `--release` mode.** The IOC runtime, Channel Access protocol handling, and areaDetector image processing involve tight loops and real-time callbacks. In debug mode, these paths run roughly 10-30x slower, which can cause CA timeouts, dropped monitor updates, and laggy waveform/image delivery. All commands below include `--release`.

### Prerequisites

```bash
# Build the entire workspace in release mode
cargo build --release --workspace
```

To interact with the running IOCs, you can use the built-in Rust CA tools (`caget-rs`, `caput-rs`, `camonitor-rs`) built as part of the workspace, or standard C EPICS clients (`caget`, `camonitor`, `cainfo`) — the wire protocol is identical.

---

### scope-ioc — Digital Oscilloscope Simulator

A port of the EPICS [testAsynPortDriver](https://github.com/epics-modules/asyn/blob/master/testAsynPortDriverApp/src/testAsynPortDriver.cpp) example. Generates a 1 kHz sine waveform (1000 points) with configurable noise, vertical gain, time/volts per division, and trigger delay. All readbacks update via I/O Intr scanning.

**Build and run:**

```bash
cargo run --release -p scope-ioc --features ioc --bin scope_ioc -- examples/scope-ioc/ioc/st.cmd
```

The IOC starts an interactive iocsh shell. You can also run the standalone demo (no CA server, just the driver logic):

```bash
cargo run --release -p scope-ioc --example scope_sim
```

**Verify with CA tools:**

```bash
# Start waveform generation
caput SCOPE:scopeSim:Run 1

# Monitor statistics
camonitor SCOPE:scopeSim:MinValue_RBV SCOPE:scopeSim:MaxValue_RBV SCOPE:scopeSim:MeanValue_RBV

# Add noise and change gain
caput SCOPE:scopeSim:NoiseAmplitude 0.2
caput SCOPE:scopeSim:VertGainSelect 3    # x10

# Read the waveform array
caget -# SCOPE:scopeSim:Waveform_RBV

# Stop
caput SCOPE:scopeSim:Run 0
```

**Open the PyDM screen:**

```bash
pydm examples/scope-ioc/opi/pydm/testAsynPortDriverTop.ui
```

---

### mini-beamline — Beamline Simulator

Inspired by [caproto's mini_beamline](https://github.com/caproto/caproto/blob/master/caproto/ioc_examples/mini_beamline.py). Simulates a complete beamline with:

- **Beam current** — sinusoidal oscillation (500 mA offset, 25 mA amplitude, 4 s period)
- **3 point detectors** — PinHole (Gaussian), Edge (error function), Slit (double error function)
- **5 motors** — SimMotor records with coordinate transforms and backlash compensation
- **MovingDot** — 2D area detector producing Gaussian spot images with Poisson noise

**Build and run:**

```bash
cargo run --release -p mini-beamline --features ioc --bin mini_ioc -- examples/mini-beamline/ioc/st.cmd
```

**Verify with CA tools:**

```bash
# Monitor beam current
camonitor mini:current

# Move the pinhole motor and watch the detector respond
caput mini:ph:mtr 0
camonitor mini:ph:DetValue_RBV
caput mini:ph:mtr 20    # move away from center — value decreases

# Acquire a MovingDot image
caput mini:dot:cam:ArrayCallbacks 1
caput mini:dot:cam:ImageMode 0          # Single
caput mini:dot:cam:AcquireTime 0.1
caput mini:dot:cam:Acquire 1
caget mini:dot:cam:ArrayCounter_RBV
```

**Open the PyDM screens:**

```bash
# Motor control
pydm crates/motor/opi/pydm/motorx_all.ui -m "P=mini:,M=ph:mtr"

# areaDetector top-level display
pydm opi/pydm/ADTop.ui -m "P=mini:,R=dot:cam:"
```

---

### sim-detector — areaDetector Simulation

A full-featured simulated areaDetector driver matching the C++ [ADSimDetector](https://github.com/areaDetector/ADSimDetector). Supports four simulation modes (LinearRamp, Peaks, Sine, OffsetNoise) with configurable gains, peak positions, and noise. Includes the full plugin chain (Stats, ROI, FFT, file writers, etc.) via `commonPlugins.cmd`.

**Build and run:**

```bash
cargo run --release --bin sim_ioc --features sim-detector/ioc -- examples/sim-detector/ioc/st.cmd
```

Or run the standalone demo (PortHandle API, no IOC):

```bash
cargo run --release -p sim-detector --example demo
```

**Verify with CA tools:**

```bash
# Set simulation mode to Peaks
caput SIM1:cam1:SimMode 1

# Acquire a single image
caput SIM1:cam1:ImageMode 0
caput SIM1:cam1:Acquire 1

# Monitor stats plugin
camonitor SIM1:Stats1:MeanValue_RBV SIM1:Stats1:MaxValue_RBV
```

**Open the PyDM screens:**

```bash
# Detector top-level display
pydm opi/pydm/ADTop.ui -m "P=SIM1:,R=cam1:"

# Detector-specific controls
pydm examples/sim-detector/opi/pydm/simDetector.ui -m "P=SIM1:,R=cam1:"

# Stats plugin
pydm opi/pydm/NDStats.ui -m "P=SIM1:,R=Stats1:"

# Image viewer
pydm opi/pydm/NDStdArrays.ui -m "P=SIM1:,R=image1:"
```

---

### seq-demo — Sequencer Demo

Demonstrates the SNL (State Notation Language) runtime with two concurrent state machines coordinating via event flags and PV monitoring.

**Build and run:**

```bash
# First, start an IOC to serve the PVs
softioc-rs --record ai:SEQ:counter --record bo:SEQ:light

# In another terminal, run the sequencer
cargo run --release -p seq-demo
```

---

### Using PyDM with epics-rs

[PyDM](https://slaclab.github.io/pydm/) (Python Display Manager) works out of the box with epics-rs because the Channel Access protocol is wire-compatible.

**Install PyDM:**

```bash
pip install pydm
# or
conda install -c conda-forge pydm
```

**General usage:**

```bash
# Launch a screen with macro substitution
pydm <path-to-ui-file> -m "P=<prefix>,R=<record>"
```

**Available PyDM screens** are distributed throughout the project:

| Location | Screens | Description |
|----------|---------|-------------|
| `opi/pydm/` | areaDetector + plugins | ADTop, Stats, ROI, FFT, file writers, etc. |
| `crates/motor/opi/pydm/` | Motor record | Motor control panels |
| `crates/asyn/opi/pydm/` | asyn record | Port driver diagnostics |
| `crates/calc/opi/pydm/` | Calc records | Transform, scalcout, sseq, userCalc |
| `crates/autosave/opi/pydm/` | Autosave | Save/restore status |
| `examples/scope-ioc/opi/pydm/` | Scope simulator | Waveform display |
| `examples/sim-detector/opi/pydm/` | SimDetector | Detector-specific controls |

When the IOC is on a different host, set the CA address list:

```bash
export EPICS_CA_ADDR_LIST="<ioc-host>"
export EPICS_CA_AUTO_ADDR_LIST=NO
pydm opi/pydm/ADTop.ui -m "P=SIM1:,R=cam1:"
```

## Binaries

### Channel Access Tools

| Binary | Description |
|--------|-------------|
| `caget-rs` | Read PV value |
| `caput-rs` | Write PV value |
| `camonitor-rs` | Subscribe to PV changes |
| `cainfo-rs` | Display PV metadata |
| `ca-repeater-rs` | CA name resolver |

### pvAccess Tools (experimental)

| Binary | Description |
|--------|-------------|
| `pvaget-rs` | PVA read |
| `pvaput-rs` | PVA write |
| `pvamonitor-rs` | PVA subscribe |
| `pvainfo-rs` | PVA metadata |

### IOC & Tools

| Binary | Description |
|--------|-------------|
| `softioc-rs` | Soft IOC server |
| `snc` | SNL compiler |
| `msi-rs` | Macro substitution tool (cli feature) |

## Feature Flags

| Crate | Feature | Default | Description |
|-------|---------|---------|-------------|
| `asyn-rs` | `epics` | no | Enable epics-base adapter bridge |
| `calc-rs` | `numeric` | yes | Numeric expression engine |
| `calc-rs` | `string` | no | String expressions |
| `calc-rs` | `array` | no | Array expressions |
| `calc-rs` | `math` | no | Advanced math functions (diff, fitting, interpolation) |
| `calc-rs` | `epics` | no | EPICS record integration (transform, scalcout, sseq) |
| `ad-core` | `ioc` | no | IOC support (includes epics-base) |
| `ad-plugins` | `parallel` | yes | Rayon data-parallelism for CPU-heavy plugins |
| `ad-plugins` | `ioc` | no | Plugin IOC support |
| `ad-plugins` | `hdf5` | no | HDF5 file plugin (requires system HDF5 library) |
| `msi-rs` | `cli` | no | `msi-rs` CLI binary |

## Testing

```bash
# All tests (1,290+)
cargo test --workspace

# With optional features
cargo test --workspace --features calc-rs/epics,asyn-rs/epics
```

Test coverage: protocol encoding, wire format golden packets, snapshot generation, GR/CTRL metadata serialization, record processing, link chains, calc engine, .db parsing, access security, autosave, iocsh, IOC builder, event scheduling, motor state machine, asyn port driver, etc.

## Requirements

- Rust 1.70+
- tokio runtime

### Optional System Dependencies

| Feature | Library | Installation |
|---------|---------|--------------|
| `ad-plugins/hdf5` | HDF5 C library | `brew install hdf5` (macOS) / `apt install libhdf5-dev` (Debian) |

All crates except the `hdf5` feature are pure Rust and require no system libraries.

## License

The Rust code authored in this repository is licensed under MIT. See
[`LICENSE`](LICENSE).

This repository also reimplements and, in a few places, bundles material from
EPICS-related upstream projects. See [`THIRD_PARTY_LICENSES`](THIRD_PARTY_LICENSES)
for attribution notices, modification notices, and the applicable upstream
license texts.
