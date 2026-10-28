# asyn-rs

Rust port of [EPICS asyn](https://github.com/epics-modules/asyn) R4-46 — an async device I/O framework for hardware drivers.

No C dependencies. Pure Rust. The record/device-support side is the `epics`
feature, **on by default**; `--no-default-features` leaves the driver
framework on its own, with no dependency on `epics-base-rs`.

**Repository:** <https://github.com/epics-rs/epics-rs>

## Overview

asyn-rs provides the same driver model as C asyn, but uses Rust's type system and async concurrency for safety and performance:

- **PortDriver trait** — implement `read_*`/`write_*` for your hardware
- **ParamList** — named parameter cache with change tracking and alarm status
- **InterruptManager** — inline sync callbacks (C `registerInterruptUser`),
  per-client mailboxes and per-subscriber rings
- **PortManager** — registry of named port drivers
- **AsynDeviceSupport** — the universal asyn device support, bridging any
  asyn-rs driver to `epics-base-rs` records (`epics` feature)

## Execution model

A driver is **not** shared behind a lock. Each one is owned exclusively by an
actor, and callers hold handles:

- **PortActor / PortRuntimeHandle** — the actor (crate-private) owns the
  driver, dispatches requests from a channel, broadcasts `RuntimeEvent`
  (Started/Stopped/Connected/Disconnected/Error) and shuts down gracefully
- **PortHandle** — a cloneable async handle with typed methods
  (`read_int32()`, `write_float64()`, …)
- **AsyncCompletionHandle** — `Future`, plus `wait_blocking()` for sync callers
- **Supervision** — a restart loop with a configurable policy
  (`max_restarts`, `restart_window`)

A port's name is claimed before its driver is built (`claim_port_driver`,
as `asynPortDriver`'s constructor calls `registerPort` first);
`PortManager::register_port(claim, driver, owner)` builds the runtime,
publishes the port under the database gate `owner`, and hands back a
`PortRuntimeHandle`.

### Protocol and transport

Inside the crate, every boundary carries pure data — no trait objects, no
closures — so a request can one day cross a process. These types are
crate-private:

| Type | Description |
|------|-------------|
| `PortCommand` | one variant per `RequestOp` |
| `PortReply` | response envelope with a typed `ReplyPayload` |
| `ParamValue` | the serializable value union |
| `PortRequest` | request envelope with `RequestMeta` |
| `PortEvent` | value change / exception, with `EventPayload` |

All of them derive `serde::Serialize`/`Deserialize`. `RuntimeClient` is the
transport seam over them; `InProcessClient` is the zero-cost path that passes
the enums straight through, and a socket client is where a multi-process
deployment would plug in.

### Typed interfaces

`InterfaceType` converts both ways against the C interface names
(`"asynInt32"` ↔ `InterfaceType::Int32`). A port's `InterfaceMask` is the one
its C constructor registers, given to `PortDriverBase::new`, and
`PortHandle::has_interface()` answers C `findInterface` from it.

## Ported drivers

`drivers/` carries the C asyn port drivers:

| driver | state |
|---|---|
| `ip_port`, `ip_server_port` | TCP/UDP client and server ports |
| `serial_port` | POSIX termios, with a Win32 DCB backend behind the same module path — C asyn's `drvAsynSerialPort.c` / `…Win32.c` split |
| `prologix` | the Prologix GPIB-Ethernet controller |
| `null_port` | the discard port |
| `ftdi` (`ftdi-mpsse`), `usbtmc` (`usbtmc`), `vxi11` (`vxi11`) | scaffolds: they compile with the feature off and `connect()` fails fast with "feature not enabled" rather than silently doing nothing |

`interpose/` is the octet-level middleware chain — `eos`, `echo`, `delay`,
`flush`, `com` — layered under a port the way C's `asynInterposeXxx` is.

## Architecture

```
┌─────────────────────────────────────────────┐
│  EPICS Records (ai, ao, longin, ...)        │
│         ↕ DeviceSupport trait                │
│  ┌─────────────────────────────────┐        │
│  │  AsynDeviceSupport (adapter)    │ epics   │
│  │  - alarm/timestamp propagation  │ feature │
│  │  - I/O Intr scan bridging       │         │
│  └──────────┬──────────────────────┘        │
└─────────────┼───────────────────────────────┘
              ↕
┌─────────────────────────────────────────────┐
│  RuntimeClient trait (transport layer)       │
│  ├── InProcessClient (zero-cost fast path)  │
│  └── [UnixSocketClient] (future)            │
│         ↕ PortCommand / PortReply            │
│  ┌─────────────────────────────────┐        │
│  │  PortRuntime / PortActor        │        │
│  │  - exclusive driver ownership   │        │
│  │  - RuntimeEvent broadcast       │        │
│  │  - graceful shutdown            │        │
│  └──────────┬──────────────────────┘        │
└─────────────┼───────────────────────────────┘
              ↕
┌─────────────────────────────────────────────┐
│  PortDriver trait                            │
│  - read/write: Int32, Float64, Octet,       │
│    UInt32Digital, arrays                     │
│  - InterfaceType / InterfaceMask            │
│                                              │
│  PortDriverBase                              │
│  ├── ParamList (cache + change tracking)     │
│  ├── InterruptManager (callbacks + rings)    │
│  └── interface / interrupt masks             │
└─────────────────────────────────────────────┘
              ↕
┌─────────────────────────────────────────────┐
│  Your Hardware Driver                        │
│  - Background async task polls device         │
│  - set_*_param() + call_param_callbacks()    │
│  - Default read_* returns cached values      │
└─────────────────────────────────────────────┘
```

## Quick Start

Add to `Cargo.toml`:

```toml
[dependencies]
asyn-rs = "0.32"
# Driver framework only, no record system:
# asyn-rs = { version = "0.32", default-features = false }
```

### Implementing a Driver

```rust
use asyn_rs::param::ParamType;
use asyn_rs::port::{InterfaceMask, PortDriver, PortDriverBase, PortFlags};
use asyn_rs::error::AsynResult;

struct TemperatureDriver {
    base: PortDriverBase,
    temp_idx: usize,
}

impl TemperatureDriver {
    fn new() -> Self {
        // C's interfaceMask and interruptMask.
        let mask = InterfaceMask::DRV_USER | InterfaceMask::FLOAT64;
        let mut base = PortDriverBase::new("tempPort", 1, PortFlags::default(), mask, mask);
        let temp_idx = base.create_param("TEMPERATURE", ParamType::Float64).unwrap();
        Self { base, temp_idx }
    }

    /// Call from a background task to update the cached value.
    fn update_temperature(&mut self, value: f64) -> AsynResult<()> {
        self.base.set_float64_param(self.temp_idx, 0, value)?;
        self.base.call_param_callbacks(0)?;
        Ok(())
    }
}

impl PortDriver for TemperatureDriver {
    fn base(&self) -> &PortDriverBase { &self.base }
    fn base_mut(&mut self) -> &mut PortDriverBase { &mut self.base }
}
```

### Registering with PortManager

```rust
use asyn_rs::manager::{PortManager, claim_port_driver};
use asyn_rs::registry::open_owner;

let manager = PortManager::new();
// The name first, as asynPortDriver's constructor does.
let claim = claim_port_driver("tempPort")?;
// The driver moves into its actor; what comes back is a handle to it.
// In an IOC `owner` is the database's `scan_control().interrupt_gate()`;
// `open_owner()` is a gate already open, for a port with no database.
let runtime = manager.register_port(claim, TemperatureDriver::new(), &open_owner())?;

// Anywhere else, by name:
let port = manager.find_port_handle("tempPort")?;
```

### EPICS Integration

With the `epics` feature, use `AsynDeviceSupport` to bridge drivers to
`epics-base-rs` records:

```rust
use asyn_rs::adapter::{AsynDeviceSupport, parse_asyn_link};

// In a DeviceSupport factory:
let link = parse_asyn_link("@asyn(tempPort, 0, 1.0) TEMPERATURE").unwrap();
let handle = manager.find_port_handle(&link.port_name)?;
let adapter = AsynDeviceSupport::from_handle(handle, link, "asynFloat64");
```

The adapter handles:
- Parameter resolution via `drvUserCreate`
- Value read/write through the port driver's cache
- Alarm status/severity propagation from driver to record
- Timestamp propagation (driver-supplied or auto-generated)
- I/O Intr scan support (a callback inline in the driver's notify, as C's
  `interruptCallbackInput`)

## Modules

| Module | Description |
|--------|-------------|
| `error` | `AsynStatus`, `AsynError` error types |
| `param` | `ParamList` — named parameter cache with types and change tracking |
| `port` | `PortDriverBase` + `PortDriver` trait with cache-based I/O defaults; the port's time stamp (`update_time_stamp`/`get_time_stamp`/`set_time_stamp`) |
| `interrupt` | `InterruptManager` — sync callbacks, mailboxes and rings |
| `manager` | `PortManager` — named port driver registry + runtime registration |
| `user` | `AsynUser` — per-request context (reason, addr) |
| `trace` | `asyn_trace!` macro for debug logging |
| `interfaces` | `InterfaceType`, `InterfaceMask`, and one module per asyn interface — int32/int64/uint32Digital/uint64/float64/octet/enum/arrays/average/gpib/motor/genericPointer |
| `drivers` | the ported port drivers (see above) |
| `interpose` | the octet interpose chain — eos, echo, delay, flush, com |
| `sync_io` | the synchronous convenience API over a port's I/O |
| `services` | the services every port is born with — C's `pasynBase` |
| `registry` | the process-wide port registry: the single claim on a port name |
| `iocsh` | the asyn iocsh commands |
| `asyn_record` | the `asyn` record and its I/O Intr support *(requires `epics`)* |
| `timestamp` | named time-stamp sources — C's `registryFunctionFind` |
| `exception` | port exception callbacks |
| `port_handle` | `PortHandle` — cloneable async handle with typed convenience methods |
| `runtime` | `PortRuntimeHandle`, `create_port_runtime`, supervision, `RuntimeEvent` lifecycle, async runtime facade (`sync`, `task`, `select!`) |
| `adapter` | `AsynDeviceSupport` — the record bridge *(requires `epics`)* |
| `snl` | testIPServerApp's `ipSNCServer.st`, compiled by snc-rs and run on epics-seq-rs, with its `db/testIPServer.db`; `register_test_ip_server_dbd` makes it startable by `seq` *(requires `epics`)* |

## Runtime Facade

asyn-rs re-exports async runtime primitives so driver authors never depend on tokio directly:

```rust
use asyn_rs::runtime::sync::{mpsc, Notify, Arc};    // channels, sync primitives
use asyn_rs::runtime::task::{spawn, sleep, interval}; // task utilities
use asyn_rs::runtime::select;                         // async multiplexing
```

For IOC binaries, use `#[epics_base_rs::epics_main]` instead of `#[tokio::main]`, and `#[epics_base_rs::epics_test]` instead of `#[tokio::test]`.

## I/O Model

The default `PortDriver` methods are `asynPortDriver`'s parameter-library I/O:

1. A background task polls the hardware
2. Driver calls `set_*_param()` to update cached values
3. Driver calls `call_param_callbacks()` to notify subscribers
4. Default `read_*` methods return the cached value

`PortFlags::can_block` is C's `ASYN_CANBLOCK`. A port that sets it queues
requests to its own thread (C `queueRequest`) and its records complete
asynchronously; one that does not runs each request inline in the record's
process.

## Testing

```bash
cargo nextest run -p asyn-rs                          # with `epics`, the default
cargo nextest run -p asyn-rs --no-default-features    # framework only
cargo bench -p asyn-rs                                # criterion throughput
```

## License

[EPICS Open License](../../LICENSE)
