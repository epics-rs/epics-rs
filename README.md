# epics-rs

Pure Rust implementation of the [EPICS](https://epics-controls.org/) control system framework.

No C dependencies. No `libca`. No `libCom`. Just `cargo build`.

**100% wire-compatible** with C EPICS clients (`caget`, `camonitor`, CSS, etc.).

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

## Examples

### scope-ioc — Digital Oscilloscope Simulator

1 kHz sine wave (1000 points), noise/gain/trigger settings. asyn PortDriver-based.

```bash
cargo run --bin scope_ioc
```

### mini-beamline — Beamline Simulator

Beam current simulator, 3 point detectors, MovingDot 2D area detector, 5-axis motor records.

```bash
cargo run --bin mini_ioc
```

### sim-detector — areaDetector Simulation

Simulated areaDetector driver IOC.

```bash
cargo run --bin sim_ioc --features sim-detector/ioc
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
