# epics-rs

Pure Rust implementation of the [EPICS](https://epics-controls.org/) control system framework.

No C dependencies. No `libca`. No `libCom`. Just `cargo build`.

## Why

EPICS is the proven standard for large-scale control systems, but standing up
a full simulation environment in C EPICS means building Base and each support
module in dependency order, wiring `RELEASE` paths, `.dbd` registrations, and
`Makefile` rules. In epics-rs the entire stack — Channel Access and pvAccess
protocols, IOC runtime, asyn, motor, areaDetector plugins — is one Cargo
workspace and one command:

```bash
cargo build --release --workspace
```

The wire protocol is identical to C EPICS, so existing clients (`caget`,
`camonitor`, `pvget`, CSS, PyDM, Phoebus) work without modification. The goal
is not to replace C EPICS in production facilities, but to provide a **fast
path from idea to running simulation** — the sim-detector example boots 8,367
records with the full areaDetector plugin chain from a single `cargo build`.

## What's included

- **Channel Access** — client & server (UDP name resolution + TCP circuit, beacons, repeater)
- **pvAccess** — client & server (search engine, monitor/get/put/RPC, ORIGIN_TAG forwarding)
- **QSRV bridge** — records as NTScalar/NTEnum/NTNDArray and Group PVs (C++ QSRV JSON compatible), `pva://` / `ca://` database links (pvalink/calink)
- **IOC runtime** — 35 base record types, .db loader, link chains, scan scheduling, ACF access security, iocsh, autosave, calc engine
- **asyn** — actor-based async port driver framework
- **motor** — 9-phase state machine, coordinate transforms, backlash compensation
- **areaDetector** — NDArray, driver base, 26 plugins (Stats/ROI/FFT/file writers/PVA push/…)
- **synApps modules** — std (epid/throttle/timestamp), scaler, optics, mca
- **Drivers** — MQTT broker bridge, Modbus TCP/RTU/ASCII
- **Targets** — Linux/macOS/Windows (x86_64 + arm64), RTEMS 6, VxWorks 7, Linux PREEMPT_RT

## Installation

All crates are published on [crates.io](https://crates.io/crates/epics-rs).
The umbrella crate pulls in what you select by feature:

```toml
[dependencies]
epics-rs = { version = "0.27", features = ["ad"] }
```

```rust
use epics_rs::base;        // IOC runtime, records, iocsh
use epics_rs::ad_core;     // NDArray, driver base
use epics_rs::ad_plugins;  // Stats, ROI, HDF5, ...
use epics_rs::asyn;        // port driver framework
```

| Feature | Description | Default |
|---------|-------------|---------|
| `ca` | Channel Access client & server | **yes** |
| `pva` | pvAccess client & server | no |
| `bridge` | Record ↔ PVA bridge (QSRV equivalent) | no |
| `asyn` | Async port driver framework | no |
| `motor` | Motor record + SimMotor | no |
| `ad` | areaDetector (core + plugins) | no |
| `ioc` | areaDetector IOC support (records, iocsh) | no |
| `std` | Standard records (epid, throttle, timestamp) | no |
| `scaler` | Scaler record (64-channel counter) | no |
| `optics` | Optics (table, monochromator, slit, filter, BPM) | no |
| `full` | Everything above | no |

`calc`, `autosave`, and `busy` are always available through `epics-base-rs`.
The `mqtt-rs`, `epics-modbus-rs`, and `mca-rs` drivers are not surfaced
through the umbrella crate — depend on them directly when needed.

You can also depend on sub-crates directly:

```toml
epics-base-rs = "0.27"  # just the IOC runtime
epics-ca-rs   = "0.27"  # just Channel Access
```

## Workspace

| Crate | Description |
|-------|-------------|
| `epics-rs` | Umbrella crate (feature-gated re-exports) |
| `epics-base-rs` | IOC core: record system, database, iocsh, calc, autosave |
| `epics-libcom-rs` | Runtime/socket layer (task seam, priority bands, errlog, net) — re-exported by `epics-base-rs` as `runtime`/`net` |
| `epics-ca-rs` | Channel Access protocol (client + server) |
| `epics-pva-rs` | pvAccess protocol (client + server) |
| `epics-bridge-rs` | Record ↔ PVA bridge (QSRV), CA/PVA gateways, pvalink |
| `epics-macros-rs` | `#[derive(EpicsRecord)]` proc macro |
| `epics-tools-rs` | Operational tooling (procserv-rs) |
| `epics-rtems-boot` | RTEMS boot shim + link contract |
| `epics-oracle-rs` | Differential oracle vs the C `softIoc` (local harness, needs a C EPICS tree) |
| `asyn-rs` | Async device I/O framework (port driver model) |
| `motor-rs` | Motor record + SimMotor |
| `ad-core-rs` | areaDetector core (NDArray, NDArrayPool, driver base) |
| `ad-plugins-rs` | 26 NDPlugins (Stats, ROI, FFT, TIFF, JPEG, HDF5, NeXus, PVA, …) |
| `std-rs` | std module (epid, throttle, timestamp) |
| `scaler-rs` | Scaler record (64-channel counter) |
| `optics-rs` | Optics (table, monochromator, slit, filter, BPM) |
| `mca-rs` | Multichannel analyzer record |
| `mqtt-rs` | MQTT broker bridge (FLAT/JSON payloads) |
| `epics-modbus-rs` | Modbus TCP/RTU/ASCII driver |

Examples under `examples/`: `scope-ioc` (oscilloscope simulator),
`mini-beamline` (DCM/slit/BPM/detectors), `sim-detector` (areaDetector
simulation), `xrt-beamline` (ray tracing), `qsrv-ioc` (Group PV demo),
`mqtt-ioc`, `modbus-ioc`, `ophyd-test-ioc` (bluesky/ophyd test PVs),
`regression-ioc` (end-to-end wire-behavior pins), `rt-probe` (PREEMPT_RT
measurement). Each has its own README; build everything with
`cargo build --release --workspace` and run e.g.

```bash
cargo run --release -p scope-ioc --features ioc --bin scope_ioc -- examples/scope-ioc/ioc/st.cmd
```

PyDM screens ship under `opi/`, `crates/*/opi`, and `examples/*/opi` — the CA
wire format is identical, so PyDM/CSS work out of the box.

## Quick start

### Run a soft IOC

```bash
cargo build --release --workspace
export PATH="$PWD/target/release:$PATH"

softioc-rs --pv TEMP:double:25.0 --pv MSG:string:hello   # simple PVs
softioc-rs --db my_ioc.db -m "P=TEST:,R=TEMP"            # from a .db file
```

### Client tools

Drop-in replacements for the C tools, byte-for-byte default output and the
full C flag set:

```bash
caget-rs TEMP        caput-rs TEMP 42.0      camonitor-rs TEMP     cainfo-rs TEMP
pvget-rs TEMP        pvput-rs TEMP 42.0      pvmonitor-rs TEMP     pvinfo-rs TEMP
```

Also shipped: `ca-repeater-rs`, `ca-gateway-rs`, `pva-gateway-rs`,
`dual-gateway-rs`, `qsrv-rs`, `procserv-rs`, `pvlist-rs`, `pvcall-rs`.

### Client library

For Rust applications talking to existing IOCs. Standard EPICS environment
variables (`EPICS_CA_ADDR_LIST`, `EPICS_PVA_ADDR_LIST`, …) are read at client
construction:

```rust
use epics_ca_rs::client::CaClient;

let client = CaClient::new().await?;
let (_dbr, value) = client.caget("TEMP:Setpoint").await?;
client.caput("TEMP:Setpoint", "42.0").await?;
client.camonitor("TEMP:Reading", |value| println!("update: {value}")).await?;
```

`epics_pva_rs::client::PvaClient` has the same shape for pvAccess
(`pvget`/`pvput`/`pvmonitor`/RPC). See [docs.rs/epics-ca-rs](https://docs.rs/epics-ca-rs)
and [docs.rs/epics-pva-rs](https://docs.rs/epics-pva-rs).

### IOC library

`st.cmd` uses the same syntax as C EPICS (`iocInit()` runs automatically after
the script), and the protocol runner is pluggable:

```rust
use epics_rs::base::server::ioc_app::IocApplication;
use epics_bridge_rs::qsrv::run_ca_pva_qsrv_ioc;   // or epics_rs::ca::server::run_ca_ioc

IocApplication::new()
    .register_device_support("myDriver", || Box::new(MyDeviceSupport::new()))
    .startup_script("ioc/st.cmd")
    .run(run_ca_pva_qsrv_ioc)                     // CA + PVA with QSRV bridge
    .await?;
```

Driver authors use the runtime facade instead of tokio directly —
`epics_base_rs::runtime::{sync, task, select}` and
`#[epics_base_rs::epics_main]` / `#[epics_base_rs::epics_test]`. See
[`crates/epics-base-rs/README.md`](crates/epics-base-rs/README.md) and the
`scope-ioc` / `mini-beamline` examples for complete drivers.

## Build for RTEMS (armv7-rtems-eabihf)

The workspace cross-compiles to RTEMS 6 — a tier-3 target, so it needs a
nightly toolchain with `rust-src` (plus `jq`), and `-Zbuild-std`:

```bash
# type-check the whole RTEMS closure — no cross-toolchain or BSP needed
./scripts/rtems-check.sh

# bootable CA IOC image (needs arm-rtems6-gcc on PATH and a libbsd BSP)
RTEMS_BSP_PREFIX=/path/to/bsp cargo +nightly build --release --locked \
    -Zbuild-std=std,panic_abort --target armv7-rtems-eabihf \
    -p epics-ca-rs --bin realtime-ca-ioc --no-default-features --features client-core
```

The custom target spec this workspace deviates on (`has-thread-local: true`,
`doc/rtems-tls-spec-deviation.md`) is applied automatically by a rustc-wrapper
wired in `.cargo/config.toml` — plain `cargo build` is the whole interface.
`./scripts/embedded-image.sh rtems ca` builds the same binary on the
`release-embedded` profile (strip + fat LTO), which is what a deployment ships:
4.6 MB against the dev build's 122.9 MB. The full build manual, including the
PVA/QSRV image and the spec escape hatch, is in
[`crates/epics-rtems-boot/README.md`](crates/epics-rtems-boot/README.md).

## Build for VxWorks 7 (x86_64-wrs-vxworks)

VxWorks 7 is the second embedded target, reached through the same
`epics_embedded_target` cfg rather than a second OS special case. It is also
tier-3, so it needs a nightly toolchain with `rust-src` and `-Zbuild-std`, but
being a builtin triple it needs no custom spec:

```bash
# census, then type-check the whole VxWorks closure — no SDK needed
./scripts/vxworks-check.sh

# deployable RTP image (needs the Wind River SDK: wr-cc and the RTP libs)
./scripts/embedded-image.sh vxworks ca
```

Both embedded targets take their `libc` fixes from one pinned public fork
branch (`physwkim/libc`, `epics-rs-0.2`). A manifest `[patch.crates-io]` does
not reach `-Zbuild-std`, which resolves std against rust-src's own lock, so
`scripts/libc-std-patch.sh` derives a config-level patch from that same pin;
`vxworks-check.sh` and `embedded-image.sh` both call it, leaving the manifest
line the single source of truth for what libc is compiled. That is what lets
the closure be type-checked on a stock nightly, CI included. Linking stays on a
box with the SDK — producing or booting a `.vxe` is the one half no runner can
do. The target contract, the cfg architecture, and what was measured on target
(gate rows, CA/PVA round-trips, image sizes) are in
[`doc/vxworks-port.md`](doc/vxworks-port.md).

## Run on RT Linux (PREEMPT_RT)

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

## Architecture

The wire format is byte-identical to C EPICS; the internals are not a
transliteration:

| Aspect | C EPICS | epics-rs |
|--------|---------|----------|
| Concurrency | POSIX threads + mutex pool | Async runtime + per-driver actor (exclusive ownership) |
| Device drivers | C functions + `void*` | Traits + typed message enums |
| Metadata | Flat record struct memory | On-demand `Snapshot` (value + alarm + time + GR/CTRL) |
| Record side effects | Direct `dbPutLink`/callback calls | Pure state machines returning `ProcessAction` data |
| Module system | `.dbd` + `Makefile` | Cargo workspace + feature flags |
| IOC configuration | `st.cmd` | Same `st.cmd` syntax, or a Rust builder API |

Per-crate design detail lives in each crate's README and on docs.rs.

## Testing

```bash
# without a C EPICS tree (what CI runs)
cargo nextest run --workspace --exclude epics-oracle-rs

# full suite, 10,000+ tests — includes the differential oracle
cargo nextest run --workspace
```

Coverage includes wire-format golden packets (CA + PVA), pvxs interop
fixtures, record processing and link chains, 46 golden tests against compiled
C `tableRecord.c` output, and `examples/regression-ioc` — an end-to-end IOC
that pins fixed wire behavior across releases.

Async tests use `#[epics_test]`, whose driver follows the build's backend:
re-running a crate's suite with `--features rtems-exec-model` exercises the
same test bodies on the reactor-free exec backend that RTEMS uses.

`epics-oracle-rs` is the differential oracle: it boots a C `softIoc` and the
Rust IOC on the same `.db` and diffs their observable CA/PVA behavior. It
needs a built C EPICS tree (point `EPICS_BASE_BIN` / `EPICS_ORACLE_DBD` /
`PVXS_BIN` at it — see
[`crates/epics-oracle-rs/README.md`](crates/epics-oracle-rs/README.md)) and
fails loudly rather than skipping when the tree is absent, so CI excludes it.
**Before contributing** changes that touch record, CA, or PVA behavior, run
the full suite *including* the oracle against a local C tree — it is the gate
CI cannot run for you.
See [`CHANGELOG.md`](./CHANGELOG.md) for the release-by-release audit trail.

## Requirements

- Rust 1.85+ (edition 2024)

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
