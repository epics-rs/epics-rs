# epics-rs

Pure Rust implementation of the [EPICS](https://epics-controls.org/) control system framework.

No C dependencies. No `libca`. No `libCom`. Just `cargo build`.

## Motivation

EPICS is the proven standard for large-scale control systems at accelerator facilities, synchrotron light sources, fusion experiments, and beyond. Its ecosystem of support modules — asyn, motor, areaDetector, calc, sequencer, autosave, and many more — represents decades of field-tested engineering.

A recurring need in controls work is an environment where **every device can be simulated in software** — motors, detectors, beam diagnostics — all running together on a single laptop without any real hardware. EPICS already supports this through simulation drivers, but the path to get there involves building EPICS Base, then each support module in dependency order, configuring `RELEASE` paths between them, writing `.dbd` registrations, and wiring `Makefile` rules. For experienced EPICS developers this is routine work, but it adds up when the goal is simply to prototype a new driver or test a control sequence.

To give a concrete example: the sim-detector IOC in this project boots with **8,367 records** (5,323 with device support, 2,991 I/O Intr scanned). Reaching that scale in C EPICS means building and linking EPICS Base, asyn, areaDetector core, and every plugin (Stats, ROI, FFT, file writers, overlay, etc.) — each with its own `configure/RELEASE`, `Makefile`, and `.dbd` wiring. In epics-rs, the same full-featured areaDetector plugin environment is a single `cargo build`.

epics-rs takes a different approach to this setup problem by leveraging Rust's Cargo package system. All support modules live in a single workspace, dependencies are declared in `Cargo.toml`, and the entire stack — from Channel Access protocol to areaDetector plugins — builds with one command:

```bash
cargo build --release --workspace
```

The wire protocol is identical to C EPICS, so existing clients (`caget`, `camonitor`, `pvget`, `pvmonitor`, CSS, PyDM, Phoebus) work without modification. The goal is not to replace C EPICS in production facilities, but to provide a **fast path from idea to running simulation** — where the focus stays on device logic rather than build infrastructure.

## Overview

epics-rs reimplements the core components of C/C++ EPICS in Rust:

- **Channel Access protocol** — client & server (UDP name resolution + TCP virtual circuit, beacons, repeater)
- **pvAccess protocol** — client & server (search engine, monitor/get/put/RPC, multi-server-on-one-host via ORIGIN_TAG forwarding)
- **QSRV bridge** — record ↔ pvAccess bridge, exposes records as NTScalar/NTEnum/NTNDArray and Group PVs (C++ QSRV JSON format compatible)
- **IOC runtime** — 35 base record types + .db file loader, link chains, scan scheduling, access security (ACF), iocsh
- **asyn framework** — actor-based async port driver model
- **Motor record** — 9-phase state machine, coordinate transforms, backlash compensation
- **areaDetector** — NDArray, driver base, 26 plugins (Stats/ROI/FFT/file writers/codec/PVA push/…)
- **Optics** — 6-DOF table record, monochromator/slit/filter/BPM controllers, X-ray absorption data
- **Standard records** — epid (PID/MaxMin feedback), throttle (rate-limited output), timestamp
- **Scaler** — 64-channel counter with presets, auto-count, delayed start
- **MQTT** — MQTT broker bridge (FLAT/JSON payloads, bidirectional)
- **Calc engine** — numeric/string/array expressions
- **Autosave** — PV save/restore

## Installation

**Current release: v0.24.2** — the `v0.20.x` line completes a full C-parity
sweep of the motor record against `epics-modules/motor` and adds per-field
DBE monitor event masks end to end, then layers ~60 commits of C-parity
regression fixes (one commit per finding) across base/db, CA, the native PVA
protocol, the QSRV/bridge gateway, asyn, motor, and the std / scaler / optics
modules. `v0.21.0` bumps the HDF5 stack (`rust-hdf5` 0.3.x, `parallel`
feature) with an opt-in no-fsync fast-close for the HDF5 writer. `v0.22.x`
removes the position-compare-output (PCO) motor surface — it had mirrored an
*unmerged* upstream `motor` PR, so it is not yet base API to track (breaking)
— and adds the `asynOctetSetInputEos` / `asynOctetSetOutputEos` iocsh
commands. `v0.23.0` makes `dbLoadRecords` `DTYP=` a plain macro instead of a
force-override (breaking: `db_loader::override_dtyp` is removed) and fixes
the AdIoc st.cmd surface — asyn iocsh commands, `$(ADCORE)` path resolution,
record-owned INP/OUT link text, per-frame `NumCaptured_RBV` in stream mode.
`v0.24.0` lands a full C-parity hardening pass driven by a new differential
oracle that boots a C `softIoc` alongside the port and diffs their CA/PVA
behaviour — ~800 one-finding-per-commit fixes across asyn, the calc engines,
the record/db metadata rsets, CA, and PVA/QSRV2 — plus two breaking asyn API
changes (`ParamSetValue` folded into `ParamSetValue::Value`, and
`drv_user_create` takes a `&DrvUserRequest`) and new oracle PVA-monitor and
array differential phases. `v0.24.1` closes the Type3 differential-oracle
parity gap — sseq/mbbo/aSub timestamp and monitor fixes, `alarm.message`
serving the record's own `amsg`, and DTYP/BOUT/QSRV2 oracle coverage — leaving
the differential harness DEFECT 0 across all three phases (additive API only,
no breaking changes). `v0.24.2` is a patch: two DTYP-resolution fixes in
`epics-base-rs` (the `dbpf` device-support path now validates and stores against
the merged declared+contributed device menu, so `dbpf <rec>.DTYP <name>` in an
st.cmd no longer aborts iocInit), plus the CBUG-B25 documentation reclassification
now that ADCore #596 landed upstream. See [`CHANGELOG.md`](./CHANGELOG.md) for
the full audit trail.

All crates are published on [crates.io](https://crates.io/crates/epics-rs). Add `epics-rs` with the feature flags you need:

```toml
[dependencies]
epics-rs = { version = "0.24", features = ["ad"] }
```

This single dependency pulls in everything needed. In your code:

```rust
use epics_rs::base;        // IOC runtime, records, iocsh
use epics_rs::ad_core;     // NDArray, driver base
use epics_rs::ad_plugins;  // Stats, ROI, HDF5, ...
use epics_rs::asyn;        // port driver framework
```

### Feature Flags

| Feature | Description | Default |
|---------|-------------|---------|
| `ca` | Channel Access client & server | **yes** |
| `pva` | pvAccess client & server | no |
| `bridge` | Record ↔ PVA bridge (QSRV equivalent) | no |
| `asyn` | Async port driver framework | no |
| `motor` | Motor record + SimMotor | no |
| `ad` | areaDetector (core + plugins) | no |
| `ioc` | areaDetector IOC support (records, iocsh) | no |
| `calc` | Calc expression engine | always |
| `autosave` | PV save/restore | always |
| `busy` | Busy record | always |
| `std` | Standard records (epid, throttle, timestamp) | no |
| `scaler` | Scaler record (64-channel counter) | no |
| `optics` | Optics (table, monochromator, slit, filter, BPM) | no |
| `full` | Everything above | no |

> The `mqtt` driver is not surfaced through the umbrella crate. Depend on `mqtt-rs = "0.24"` directly when needed.
>
> The Modbus driver is not surfaced through the umbrella crate either.
> Depend on `epics-modbus-rs = "0.24"` directly when needed; the Rust
> library name is `modbus_rs`, so consumers write `use modbus_rs::...`.

```toml
# Motor + areaDetector
epics-rs = { version = "0.24", features = ["motor", "ad"] }

# Everything
epics-rs = { version = "0.24", features = ["full"] }
```

### Individual Crates

You can also depend on sub-crates directly if you only need specific functionality:

```toml
[dependencies]
ad-plugins-rs = "0.24"  # just the areaDetector plugins
epics-base-rs = "0.24"  # just the IOC runtime
```

## Workspace Structure

```
epics-rs/
├── crates/
│   ├── epics-rs/         # Umbrella crate (feature-gated re-exports)
│   ├── epics-base-rs/    # Core: IOC runtime, 35 record types, iocsh, db loader
│   ├── epics-ca-rs/      # Channel Access protocol (client + server)
│   ├── epics-pva-rs/     # pvAccess protocol (client + server)
│   ├── epics-bridge-rs/  # Record ↔ PVA bridge (QSRV equivalent)
│   ├── epics-macros-rs/  # #[derive(EpicsRecord)] proc macro
│   ├── epics-tools-rs/   # ca*-rs / pv*-rs CLI tools (caget, pvget, …)
│   ├── asyn-rs/          # Async device I/O framework (port driver model)
│   ├── motor-rs/         # Motor record + SimMotor
│   ├── ad-core-rs/       # areaDetector core (NDArray, NDArrayPool, driver base)
│   ├── ad-plugins-rs/    # 26 NDPlugins (Stats, ROI, FFT, TIFF, JPEG, HDF5, NeXus, NetCDF, PVA, …)
│   ├── std-rs/           # Standard records (epid, throttle, timestamp) + device support
│   ├── scaler-rs/        # Scaler record (64-channel counter) + device support
│   ├── optics-rs/        # Optics (table, monochromator, slit, filter, BPM)
│   └── mqtt-rs/          # MQTT driver (broker bridge, FLAT/JSON payloads)
└── examples/
    ├── scope-ioc/        # Digital oscilloscope simulator
    ├── mini-beamline/    # Beamline simulator with DCM, slit, BPM, detectors
    ├── sim-detector/     # areaDetector simulation driver
    ├── xrt-beamline/     # X-ray beamline with real-time ray tracing (xrt-rs)
    ├── qsrv-ioc/         # QSRV group PV demo (PVA composite over CA records)
    ├── mqtt-ioc/         # MQTT IOC example
    └── ophyd-test-ioc/   # PVs expected by the bluesky/ophyd test suite
```

### Crate Dependency Graph

```
epics-rs (umbrella — feature-gated re-exports)
    │
    ├── epics-base-rs ◄─── epics-macros-rs (proc macro)
    │       ▲
    │       ├── asyn-rs
    │       │    └── motor-rs
    │       ├── ad-core-rs
    │       │    ├── asyn-rs
    │       │    └── ad-plugins-rs
    │       ├── std-rs (epid, throttle, timestamp)
    │       ├── scaler-rs (64-channel counter)
    │       ├── optics-rs (table, monochromator, slit, filter, BPM)
    │       └── mqtt-rs (MQTT broker bridge)
    │
    ├── epics-ca-rs (Channel Access protocol)
    ├── epics-pva-rs (pvAccess client + server)
    ├── epics-bridge-rs (Record ↔ PVA bridge)
    │        ├── epics-base-rs
    │        └── epics-pva-rs
    └── epics-tools-rs (procserv-rs and CLI helpers)
```

## Architecture: C EPICS vs epics-rs

### Key Design Differences

| Aspect | C EPICS | epics-rs |
|--------|---------|----------|
| **Concurrency model** | POSIX threads + mutex pool + event queue | Async runtime + per-driver actor (exclusive ownership) |
| **Record internals** | C struct fields, `dbAddr` pointer arithmetic | Rust trait system, on-demand `Snapshot` assembly |
| **Device drivers** | C functions + `void*` pointers | Rust traits + impl blocks (type-safe) |
| **Metadata storage** | Stored directly in record C struct (flat memory) | Assembled on-demand into `Snapshot` (Display/Control/EnumInfo) |
| **Module system** | `.dbd` files + `Makefile` | Cargo workspace + feature flags |
| **Link resolution** | `dbAddr` pointer offsets | Trait methods + field name dispatch |
| **Memory safety** | Manual management (segfault possible) | Safe Rust (no unsafe in record logic) |
| **IOC configuration** | `st.cmd` shell script | Rust builder API or `st.cmd`-compatible parser |
| **Wire format** | CA protocol | **Identical** (fully compatible with C clients/servers) |

### 1. Actor-Based Concurrency

C EPICS uses a global shared state with mutex pools. In epics-rs, each driver has an async actor with exclusive ownership — no `Arc<Mutex>` on the hot path:

```
C EPICS:                          epics-rs:
┌──────────────────┐              ┌──────────────────┐
│  Global State    │              │   PortActor      │ ← exclusive ownership
│  + Mutex Pool    │              │   (async task)   │
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

### 5. ProcessOutcome: Action-Based Side Effects

C EPICS records call `dbPutLink()`, `callbackRequestDelayed()`, and device support functions directly from `process()`. In epics-rs, records are pure state machines that express side effects as **action requests**:

```rust
pub enum ProcessAction {
    WriteDbLink { link_field, value },     // "write this value to that link"
    ReadDbLink { link_field, target },     // "read that link into this field" (pre-process)
    ReprocessAfter(Duration),              // "wake me up after N seconds"
    DeviceCommand { command, args },       // "tell device support to do this"
}
```

The processing layer executes these actions at the correct point in the cycle. Records never touch the database directly. This keeps records testable (unit-test `process()` by inspecting returned actions) and decoupled from the runtime infrastructure.

### 6. Record System Separation

In C EPICS, each record type requires separate `.dbd` and C source files. epics-rs splits the record system into two layers:

- **`record.rs`** — shared infrastructure for all record types (`CommonFields`, `Record` trait, `RecordInstance`, link parsing, field get/put, alarm logic)
- **`records/*.rs`** — per-record-type files. `#[derive(EpicsRecord)]` generates boilerplate

Adding a new record type requires only a new file in `records/` — no changes to `record.rs`.

## Record Types

`epics-base-rs` provides 35 base record types; the satellite crates add domain-specific records (`motor`, `table`, `scaler`, `epid`, `throttle`, `timestamp`).

### Base records (epics-base-rs)

| Group | Records |
|-------|---------|
| Analog / integer scalar | `ai`, `ao`, `longin`, `longout`, `int64in`, `int64out` |
| Binary / enum | `bi`, `bo`, `mbbi`, `mbbo`, `mbbi_direct`, `mbbo_direct`, `busy` |
| String | `stringin`, `stringout`, `lsi`, `lso`, `printf` |
| Array / waveform | `waveform`, `compress`, `histogram` |
| Calculation | `calc`, `calcout`, `scalcout`, `transform`, `swait`, `sub`, `asub` |
| Sequencing / fanout | `fanout`, `dfanout`, `seq`, `sseq`, `sel`, `event` |
| Misc | `asyn` |

### Add-on records (per crate)

| Crate | Records |
|-------|---------|
| `motor-rs` | `motor` (9-phase state machine, backlash, retries, coordinate transforms) |
| `optics-rs` | `table` (6-DOF optical table, 4 geometry modes) |
| `scaler-rs` | `scaler` (64-channel 32-bit counter, OneShot/AutoCount) |
| `std-rs` | `epid` (PID/MaxMin feedback), `throttle` (rate-limited output), `timestamp` (formatted timestamp string) |

## Quick Start

### Build

```bash
cargo build --release --workspace
```

The command-line tools (`softioc-rs`, `caget-rs`, `caput-rs`, `camonitor-rs`, `cainfo-rs`) are located in `target/release/`. Add it to your `PATH` for convenience:

```bash
export PATH="$PWD/target/release:$PATH"
```

### Build for RTEMS (armv7-rtems-eabihf)

The workspace also cross-compiles to RTEMS 6 — a tier-3 target, so it needs a
nightly toolchain with `rust-src` (plus `jq`), and `-Zbuild-std`:

```bash
# type-check the whole RTEMS closure — no cross-toolchain or BSP needed
./scripts/rtems-check.sh

# bootable CA IOC image (needs arm-rtems6-gcc on PATH and a libbsd BSP)
RTEMS_BSP_PREFIX=/path/to/bsp cargo +nightly build --release --locked \
    -Zbuild-std=std,panic_abort --target armv7-rtems-eabihf \
    -p epics-ca-rs --bin rtems-ca-ioc --no-default-features --features client-core
```

The custom target spec this workspace deviates on (`has-thread-local: true`,
`doc/rtems-tls-spec-deviation.md`) is applied automatically by a rustc-wrapper
wired in `.cargo/config.toml` — plain `cargo build` is the whole interface.
The full build manual, including the PVA/QSRV image and the spec escape hatch,
is in [`crates/epics-rtems-boot/README.md`](crates/epics-rtems-boot/README.md).

### Run on RT Linux (PREEMPT_RT)

On a PREEMPT_RT kernel the ordinary host build is the whole build — real time
is enabled at run/feature level, not by cross-compiling:

```bash
# build with priority-inheritance mutexes (PTHREAD_PRIO_INHERIT)
cargo build --release --features epics-base-rs/linux-rt

# opt the IOC into SCHED_FIFO thread banding (default is off on hosted targets),
# granting the privilege via an RTPRIO rlimit, CAP_SYS_NICE, or chrt
EPICS_RS_ALLOW_RT_PRIORITY=YES ./your-ioc
```

Both levers are documented in
[`crates/epics-base-rs/README.md`](crates/epics-base-rs/README.md) ("Real-time
deployment"); the measured evidence — PI collapsing the record-gate priority
inversion to its critical-section bound on a real PREEMPT_RT kernel — is in
`doc/rtlinux-rt-measurement.md`.

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
caput-rs TEMP 42.0         # write
camonitor-rs TEMP          # subscribe
cainfo-rs TEMP             # metadata
```

The default output of `caget-rs` / `camonitor-rs` / `caput-rs` / `cainfo-rs` matches the legacy C tools byte-for-byte (PV-name padded to 30 chars, `%g` 6-digit precision, double-space ts↔value, `Old : … New : …` echo). Full C-tool flag set is supported on every binary: `-V`, `-t`, `-a`, `-d`, `-c`, `-p`, `-n`, `-#`, `-S`, `-e`/`-f`/`-g`, `-s`, `-lx`/`-lo`/`-lb`, `-0x`/`-0o`/`-0b`, `-F`. C EPICS clients (`caget`, `camonitor`, CSS, PyDM, …) also work as-is.

### PVA Client Tools

```bash
pvget-rs TEMP              # read via pvAccess
pvmonitor-rs TEMP          # subscribe via pvAccess
pvput-rs TEMP 42.0         # write via pvAccess
pvinfo-rs TEMP             # PV type info
```

Legacy `pv*` flags are wired on every PVA tool (`-V`, `-w`, `-r`, `-p`, `-M`, `-v`, `-q`, `-d`); `-M nt` output matches `pvget` byte-for-byte. First-response latency on `pvget-rs <PV>` is 5–10 ms against a local IOC.

### Use as a CA / PVA client library

For Rust applications that need to **talk to existing EPICS IOCs**
(soft IOCs, hardware IOCs, gateways — anything speaking the CA / PVA
wire protocol). The umbrella crate or the per-protocol crates work
either way.

```toml
[dependencies]
# Client + server, both protocols (recommended for new projects):
epics-rs = { version = "0.24", features = ["pva"] }   # ca enabled by default

# Or per-protocol, no umbrella:
epics-ca-rs  = "0.24"
epics-pva-rs = "0.24"
```

Standard EPICS environment variables (`EPICS_CA_ADDR_LIST` /
`EPICS_PVA_ADDR_LIST` / `*_AUTO_ADDR_LIST` / `*_NAME_SERVERS` /
`EPICS_PVAS_TLS_KEYCHAIN` …) are read at client construction time —
nothing else to wire up if your IOCs are already discoverable.

#### Channel Access (CA)

```rust
use epics_ca_rs::client::CaClient;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let client = CaClient::new().await?;

    // Read — returns (DBR type, value)
    let (_dbr, value) = client.caget("TEMP:Setpoint").await?;
    println!("{value}");

    // Write — value parsed from the supplied string
    client.caput("TEMP:Setpoint", "42.0").await?;

    // Subscribe — callback fires on every value-change event
    client.camonitor("TEMP:Reading", |value| {
        println!("update: {value}");
    }).await?;
    Ok(())
}
```

For richer per-channel control (state changes, async wait-for-connect,
explicit DBR variants, subscription handles) use
`client.create_channel(name) -> CaChannel`. See [`docs.rs/epics-ca-rs`](https://docs.rs/epics-ca-rs).

#### pvAccess (PVA)

```rust
use epics_pva_rs::client::PvaClient;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let client = PvaClient::new()?;

    // Read — returns the full structured PvField
    let value = client.pvget("MY:Struct").await?;
    println!("{value:?}");

    // Write (string-parsed)
    client.pvput("MY:Setpoint", "42.0").await?;

    // Subscribe — callback receives each MONITOR update
    client.pvmonitor("MY:Stream", |update| {
        println!("update: {update:?}");
    }).await?;
    Ok(())
}
```

`PvaClient::builder()` exposes per-client overrides (timeout, name
servers, TLS, priority) when you don't want to thread settings
through environment variables. See [`docs.rs/epics-pva-rs`](https://docs.rs/epics-pva-rs).

#### Drop-in CLI tools

The protocol crates ship standard CLI replacements that accept the
same flag set as the C tools:

```sh
# CA tools — caget-rs / caput-rs / camonitor-rs / cainfo-rs
cargo install epics-ca-rs

# PVA tools — pvget-rs / pvput-rs / pvmonitor-rs / pvinfo-rs
cargo install epics-pva-rs

caget-rs TEMP                # read via CA
camonitor-rs TEMP            # subscribe via CA
pvget-rs TEMP                # read via PVA
pvmonitor-rs TEMP            # subscribe via PVA
```

### Library Usage

#### Declarative IOC Builder

```rust
use epics_rs::base::server::ioc_app::IocApplication;
use epics_rs::base::server::records::ao::AoRecord;
use epics_rs::base::server::records::bi::BiRecord;
use epics_rs::ca::server::run_ca_ioc;

IocApplication::new()
    .record("TEMP", AoRecord::new(25.0))
    .record("INTERLOCK", BiRecord::new(0))
    .run(run_ca_ioc)
    .await?;
```

#### IocApplication (st.cmd Style)

```rust
use epics_rs::base::server::ioc_app::IocApplication;
use epics_rs::ca::server::run_ca_ioc;

IocApplication::new()
    .register_device_support("myDriver", || Box::new(MyDeviceSupport::new()))
    .startup_script("ioc/st.cmd")
    .run(run_ca_ioc)
    .await?;
```

The protocol runner is pluggable — use `run_ca_pva_qsrv_ioc` for dual CA+PVA:

```rust
// CA + PVA simultaneously (QSRV bridge auto-wired)
use epics_bridge_rs::qsrv::run_ca_pva_qsrv_ioc;

IocApplication::new()
    .register_device_support("myDriver", || Box::new(MyDeviceSupport::new()))
    .startup_script("ioc/st.cmd")
    .run(run_ca_pva_qsrv_ioc)
    .await?;
```

st.cmd uses **the same syntax as C++ EPICS** (`iocInit()` is called automatically after the script completes):

```bash
epicsEnvSet("PREFIX", "SIM1:")
myDriverConfig("SIM1", 256, 256, 50000000)
dbLoadRecords("$(MY_DRIVER)/Db/myDriver.db", "P=$(PREFIX)")
```

#### CA Client Library

```rust
use epics_rs::ca::client::CaClient;

let client = CaClient::new().await?;
let (_type, value) = client.caget("TEMP").await?;
client.caput("TEMP", "42.0").await?;
```

#### PVA Client Library

```rust
use epics_pva_rs::client::PvaClient;

let client = PvaClient::new().await?;
let value = client.get("TEMP").await?;
client.put("TEMP", 42.0).await?;
let mut sub = client.monitor("TEMP").await?;
while let Some(update) = sub.recv().await { /* … */ }
```

### Runtime Interface

Driver authors should use the runtime facade instead of depending on tokio directly. Both `asyn-rs` and `epics-base-rs` provide the same re-exports:

```rust
// Sync primitives (channels, Notify, etc.)
use asyn_rs::runtime::sync::{mpsc, Notify, Arc};

// Task utilities (spawn, sleep, timers)
use asyn_rs::runtime::task::{spawn, sleep, interval};

// Async multiplexing
use asyn_rs::runtime::select;

// IOC entry point (replaces #[tokio::main])
#[epics_base_rs::epics_main]
async fn main() -> CaResult<()> { /* ... */ }

// Async tests (replaces #[tokio::test])
#[epics_base_rs::epics_test]
async fn test_something() { /* ... */ }
```

IOC binaries and device support implementations should use `epics_base_rs::runtime::` or `asyn_rs::runtime::`. See the [scope-ioc](examples/scope-ioc/) and [mini-beamline](examples/mini-beamline/) examples for complete driver implementations using this pattern.

## Crate Details

### epics-base-rs (core)

IOC runtime, 35 base record types, iocsh, .db file loader, access security, autosave integration, calc engine, busy record.

- Record system with `#[derive(EpicsRecord)]` proc macro
- PvDatabase with record processing chains (FLNK, INP/OUT links)
- ACF file parser (UAG/HAG/ASG rules)
- iocsh command interpreter

### epics-ca-rs

Channel Access protocol — client and server.

- UDP name resolution + TCP virtual circuit
- Extended CA header (>64 KB payloads)
- Beacon emitter with reset on connect/disconnect
- Monitor subscriptions with deadband filtering
- `EPICS_CA_ADDR_LIST` hostname resolution with graceful skip on unresolvable tokens
- Full C-tool flag parity on every CLI binary (`-V`/`-t`/`-a`/`-#`/`-S`/`-e`/`-f`/`-g`/`-s`/`-l[xob]`/`-0[xob]`/`-F`)

### epics-pva-rs

pvAccess protocol — client and server.

- **Client** — search engine (pvxs `Multi`/`FindAll` parity, deadline-aware tick), cached channels, `pvget`/`pvput`/`pvmonitor`/RPC, `pvRequest` field selection, hostname resolution in `EPICS_PVA_ADDR_LIST`
- **Server** — TCP virtual circuit, UDP responder, beacons, `CMD_DESTROY_CHANNEL` propagation, `CMD_MESSAGE` log forwarding, RPC INIT/DATA wire-shape compatible with pvxs/pvAccessCPP
- **ORIGIN_TAG forwarding** — pvxs UDP-collector mechanism for multi-server-on-one-host topologies (`224.0.0.128` loopback multicast)
- **Auto-address list** — per-NIC IPv4 broadcast + `127.0.0.1` enumeration on macOS/Linux

### epics-bridge-rs

QSRV equivalent — bridges EPICS database records to pvAccess channels:

- **Single record channels** — NTScalar, NTEnum (with choices), NTScalarArray with full metadata (alarm, timeStamp, display, control, valueAlarm)
- **Group PV channels** — composite PvStructure from multiple records (C++ QSRV JSON format compatible)
- **Monitor bridge** — full Snapshot on every update, initial snapshot on connect, fan-in group monitor with trigger rules
- **pvRequest support** — field selection, `record._options.process`/`block`
- **Group config** — external JSON files + `info(Q:group, ...)` record tags, member merge
- **Infrastructure** — ChannelProvider/Channel/PvaMonitor traits, record metadata cache, pluggable access control

### asyn-rs

Rust port of C EPICS asyn. Actor-based port driver model:

- **PortDriver trait** — `read_int32`, `write_float64`, `read_octet_array`, etc.
- **ParamList** — change tracking, timestamps, alarm propagation
- **PortActor** — exclusive driver ownership (async task)
- **PortHandle** — cloneable async interface
- **RuntimeClient** — transport abstraction (InProcessClient, future UnixSocketClient)

### motor-rs

Complete motor record implementation:

- **9-phase motion state machine** — Idle, MainMove, BacklashApproach, BacklashFinal, Retry, Jog, JogStopping, JogBacklash, Homing
- **Coordinate transforms** — User <-> Dial <-> Raw (steps)
- **Backlash compensation** — approach + final move
- **4 retry modes** — Default, Arithmetic, Geometric, InPosition
- **AxisRuntime** — per-axis async actor, poll loop
- **SimMotor** — time-based linear interpolation motor for testing

### ad-core & ad-plugins

areaDetector framework:

- **NDArray** — N-dimensional typed array (10 data types)
- **NDArrayPool** — free-list buffer reuse
- **ADDriverBase** — detector driver base (Single/Multiple/Continuous modes)
- **26 plugins** — Stats, ROI, ROIStat, Process, Transform, ColorConvert, Overlay, FFT, TimeSeries, CircularBuff, Codec, Gather, Scatter, StdArrays, FileTIFF, FileJPEG, FileHDF5, FileNeXus, FileNetCDF, FileMagick, Attribute, AttrPlot, BadPixel, PosPlugin, Passthrough, Pva (NTNDArray push)
- **Parallel processing** — rayon data-parallelism for CPU-heavy plugins (Stats, ROIStat, ColorConvert, Process). Shared thread pool sized to `available_cores - 2` to leave headroom for driver threads and the async runtime. Enabled by default; see [ad-plugins README](crates/ad-plugins-rs/README.md#parallel-processing)

#### Async acquisition API

Detector acquisition tasks are fully async. The data path has no lossy or
blocking APIs — param updates and frame publishing all use reliable async
enqueue. A driver author implementing an acquisition task only needs three
types from `ad_core_rs::plugin::channel`:

| Type | Purpose |
|------|---------|
| `PortHandle` | Read/write parameters: `read_int32().await`, `set_params_and_notify().await` |
| `ArrayPublisher` | Publish a generated frame to downstream plugins: `publisher.publish(frame).await` |
| `QueuedArrayCounter` | Wait until in-flight frames drain at end of acquisition |

`blocking_callbacks` controls completion-wait depth, not thread blocking:
- `0`: await queue admission only (downstream processes asynchronously)
- `1`: await queue admission + downstream processing completion

Fan-out to multiple plugins is concurrent — a slow downstream does not
stall sibling downstreams. See `examples/sim-detector` for a full driver
implementation.

### Calc Engine (in epics-base-rs)

Expression engine:

- **Numeric** — infix-to-postfix compilation, 16 input variables (A-P), math functions
- **String** — string manipulation, 12 string variables (AA-LL)
- **Array** — element-wise operations, statistics (mean, sigma, min, max, median)
- **EPICS records** — transform, scalcout, sseq (epics feature)

### std-rs

Port of the EPICS [std](https://github.com/epics-modules/std) synApps module:

- **epid record** — Extended PID feedback with PID and MaxMin modes, anti-windup, bumpless turn-on, output deadband
- **throttle record** — Rate-limited output with drive limits, delay enforcement, sync input
- **timestamp record** — Formatted timestamp string (11 format options)
- **Device support** — Epid Soft (synchronous PID), Epid Async Soft (trigger-based), Fast Epid (interrupt-driven 1kHz+ PID), Time of Day, Sec Past Epoch
- **SNL programs** — Femto amplifier gain control, delayDo state machine (native Rust async)
- **70+ database templates** and autosave request files bundled

### scaler-rs

Port of the EPICS [scaler](https://github.com/epics-modules/scaler) module:

- **scaler record** — 64-channel 32-bit counter with per-channel presets, gates, directions, names
- **OneShot/AutoCount** modes with configurable DLY/DLY1 delayed start
- **RATE/RAT1** periodic display update during counting
- **COUT/COUTP** output links fired on count start/stop transitions
- **Asyn device support** — bridges to ScalerDriver trait (reset, read, write_preset, arm, done)
- **Software scaler driver** — for testing/simulation

### optics-rs

Port of the EPICS [optics](https://github.com/epics-modules/optics) synApps module:

- **table record** — 6-DOF optical table with 4 geometry modes (SRI, GEOCARS, NEWPORT, PNC), motor-to-user/user-to-motor coordinate transforms, polynomial limit interpolation
- **Monochromator controllers** — Kohzu DCM (`kohzuCtl`), HR analyzer (`hrCtl`), multi-layer mono (`ml_monoCtl`) as async state machines
- **Diffractometer** — 4-circle orientation matrix (`orient`) with HKL-to-angles / angles-to-HKL
- **Filter controllers** — automatic filter selection (`filterDrive`), XIA PF4 dual filter (`pf4`) using Chantler X-ray absorption data (22 elements)
- **Device drivers** — HSC-1 slit controller (`SimHsc` / serial), quad BPM (`SimQxbpm` / serial) as asyn port drivers
- **Ion chamber** — I₀ intensity calculation with gas mixture absorption
- **`seqStart` command** — general-purpose launcher for all optics state machines (replaces C EPICS `seq`)
- **36 database templates** and PyDM UI screens bundled
- **374 tests** including 46 golden tests verified against compiled C tableRecord.c output

### Autosave (in epics-base-rs)

PV automatic save/restore:

- **C-compatible iocsh commands** — `set_requestfile_path`, `set_savefile_path`, `create_monitor_set`, `create_triggered_set`, `set_pass0_restoreFile`, `set_pass1_restoreFile`, `save_restoreSet_status_prefix`
- **Pass0/Pass1 restore** — Pass0 before device support init, Pass1 after (matching C autosave behavior)
- **Request file parsing** — `.req` files with `file` includes, macro expansion (`$(P)`, `${KEY}`, `$(KEY=default)`), environment variable fallback, search path resolution, cycle detection
- Periodic/triggered/on-change/manual save strategies
- Atomic file write (tmp -> fsync -> rename)
- Backup rotation (`.savB`, sequence files, dated backups)
- C autosave-compatible `.sav` file format
- **Runtime iocsh commands** — `fdbrestore`, `fdbsave`, `fdblist`

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
- **8 motors** — SimMotor records (5 for detectors + 3 for DCM)
- **MovingDot** — 2D area detector producing Gaussian spot images with Poisson noise
- **Kohzu DCM** — double crystal monochromator with energy→Bragg angle control
- **HSC-1 slit** — simulated 4-blade slit controller
- **Quad BPM** — simulated beam position monitor

**Build and run:**

```bash
cargo run --release -p mini-beamline --features ioc --bin mini_ioc -- examples/mini-beamline/ioc/st.cmd
```

**Verify with CA tools:**

```bash
# Monitor beam current
camonitor mini:current

# Set DCM energy and watch the theta motor
caput mini:BraggEAO 8.0
caget mini:BraggThetaRdbkAO
camonitor mini:dcm:theta.RBV

# Move the pinhole motor and watch the detector respond
caput mini:ph:mtr 0
camonitor mini:ph:DetValue_RBV
caput mini:ph:mtr 20    # move away from center — value decreases

# Acquire a MovingDot image
caput mini:dot:cam1:ArrayCallbacks 1
caput mini:dot:cam1:ImageMode 0          # Single
caput mini:dot:cam1:AcquireTime 0.1
caput mini:dot:cam1:Acquire 1
caget mini:dot:cam1:ArrayCounter_RBV
caget mini:dot:image1:ArrayData
```

**Open the PyDM screens:**

```bash
# Motor control
pydm crates/motor-rs/opi/pydm/motorx_all.ui -m "P=mini:,M=ph:mtr"

# areaDetector top-level display
pydm opi/pydm/ADTop.ui -m "P=mini:dot:,R=cam1:"
```

---

### ophyd-test-ioc — Ophyd Test Suite IOC

Provides the PVs expected by [bluesky/ophyd](https://github.com/bluesky/ophyd)'s test suite, replacing the Docker-based [epics-services-for-ophyd](https://github.com/bluesky/epics-services-for-ophyd). 9 SimMotor instances, 6 soft-channel sensors, and a MovingDot 2D area detector with the standard plugin chain.

**Build and run:**

```bash
cargo run --release -p ophyd-test-ioc --bin ophyd_test_ioc -- examples/ophyd-test-ioc/ioc/st.cmd
```

See [examples/ophyd-test-ioc/README.md](examples/ophyd-test-ioc/README.md) for the full PV list.

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
| `crates/motor-rs/opi/pydm/` | Motor record | Motor control panels |
| `crates/asyn-rs/opi/pydm/` | asyn record | Port driver diagnostics |
| `crates/optics-rs/ui/` | Optics module | DCM, slit, filter, table, orient, BPM screens |
| `crates/std-rs/ui/` | Standard module | PID, timer, shutter, misc screens |
| `crates/scaler-rs/ui/` | Scaler module | Counter displays (16/32/64 channel) |
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
| `ca-repeater-rs` | CA repeater daemon |
| `ca-replay-rs` | Capture / replay CA wire traffic |
| `ca-lint-rs` | CA wire-format diagnostics |
| `ca-admin-rs` | CA server administration |

### pvAccess Tools

| Binary | Description |
|--------|-------------|
| `pvget-rs` | PVA read |
| `pvput-rs` | PVA write |
| `pvmonitor-rs` | PVA subscribe |
| `pvinfo-rs` | PVA metadata |
| `pvcall-rs` | PVA RPC call |
| `pvlist-rs` | List PVA servers |
| `pvxvct-rs` | PVA virtual-circuit inspector |

### IOC & Gateways

| Binary | Description |
|--------|-------------|
| `softioc-rs` | Soft IOC server (CA) |
| `qsrv-rs` | QSRV-style CA + PVA dual IOC |
| `ca-gateway-rs` | CA → CA gateway |
| `pva-gateway-rs` | PVA → PVA gateway |
| `dual-gateway-rs` | CA ↔ PVA bidirectional gateway |
| `procserv-rs` | `procServ`-compatible IOC supervisor |

## Feature Flags

| Crate | Feature | Default | Description |
|-------|---------|---------|-------------|
| `asyn-rs` | `epics` | no | Enable epics-base adapter bridge |
| `ad-core-rs` | `ioc` | no | IOC support (includes epics-base) |
| `ad-plugins-rs` | `parallel` | yes | Rayon data-parallelism for CPU-heavy plugins |
| `ad-plugins-rs` | `ioc` | no | Plugin IOC support |
| `ad-plugins-rs` | `hdf5` | no | HDF5 file plugin (HDF5 2.0 built from bundled source, requires cmake) |

## Testing

```bash
# All tests (6,000+)
cargo test --workspace
```

Test coverage: protocol encoding, wire-format golden packets (CA + PVA), pvxs interop fixtures, search-engine deadline scheduling, ORIGIN_TAG forwarding, snapshot generation, GR/CTRL metadata serialization, record processing, link chains, calc engine, .db parsing, access security, autosave, iocsh, IOC builder, event scheduling, motor state machine, asyn port driver, PID algorithms, scaler state machine, optics table record (46 golden tests vs C), crystallography, X-ray absorption, monochromator/slit/filter/BPM controllers, derive macros, etc.

### Regression IOC (end-to-end)

[`examples/regression-ioc`](examples/regression-ioc/) boots a real in-process
IOC — CA + PVA servers over one shared database — and asserts fixed behavior
**over the wire**, pinning recurring bug-fix families from v0.15.x–v0.22.x
(processing-chain/FLNK, monitor-on-change, periodic SCAN, motor
move-on-Passive-VAL, enum/`DBR_ENUM`, `DBF_MENU`, alarm severity, timestamp).

```bash
cargo nextest run -p regression-ioc          # or: cargo test -p regression-ioc
cargo run -p regression-ioc --bin regression_ioc   # run the IOC by hand
```

Because `examples/*` are workspace members, these run automatically in CI under
`cargo nextest run --workspace` (`rust.yml`) and the cross-platform matrix
(`--profile ci`, `cross-platform.yml`); no external tools are needed.

## Requirements

- Rust 1.85+ (edition 2024)
- Async runtime (provided by `epics-base-rs` — no direct tokio dependency needed)

## Related projects

Companion projects that build on or pair with `epics-rs`:

- **[epics-rs-iocs](https://github.com/epics-rs/epics-rs-iocs)** — Cargo workspace of `epics-rs`-based IOC applications; each device driver is an independent library crate under `drivers/` and each IOC binary lives under `iocs/`.
- **[ophyd-epicsrs](https://github.com/physwkim/ophyd-epicsrs)** — Rust EPICS backend for bluesky's [ophyd](https://github.com/bluesky/ophyd) / [ophyd-async](https://github.com/bluesky/ophyd-async), replacing pyepics with `epics-rs` over PyO3 (CA + PVA, GIL released during network I/O).
- **[bsrs](https://github.com/physwkim/bsrs)** — Rust-native re-implementation of the bluesky acquisition stack (RunEngine, devices, plans, document sinks), removing the Python requirement on IOC hosts.
- **[archiver-rs](https://github.com/physwkim/archiver-rs)** — High-performance EPICS Channel Access archiver in Rust, compatible with the Java EPICS Archiver Appliance data format and REST API.

Related scientific tooling from the same author:

- **[rsplot](https://github.com/physwkim/rsplot)** — silx-style scientific plotting for [egui](https://github.com/emilk/egui), GPU-rendered with wgpu; a Rust port of `silx.gui.plot`.
- **[tomoxide](https://github.com/physwkim/tomoxide)** — Rust tomographic reconstruction toolkit fusing [tomopy](https://github.com/tomopy/tomopy)'s algorithmic breadth with [tomocupy](https://github.com/tomography/tomocupy)'s streaming reconstruction across a tri-backend CPU / CUDA / wgpu abstraction.

## Development Note

AI-assisted tools were used in parts of this project.
All changes are reviewed and tested by human maintainers.
Final responsibility for correctness of the port remains with the maintainers.

## License

This software is distributed under the [EPICS Open License](LICENSE), the same
license used by EPICS Base and most EPICS community modules.

This repository also reimplements and, in a few places, bundles material from
EPICS-related upstream projects. See [`THIRD_PARTY_LICENSES`](THIRD_PARTY_LICENSES)
for attribution notices, modification notices, and the applicable upstream
license texts.
