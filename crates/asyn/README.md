# asyn-rs

Rust port of [EPICS asyn](https://epics-modules.github.io/master/asyn/R4-44/asynDriver.html) — an async device I/O framework for hardware drivers.

No C dependencies. Pure Rust. Integrates with [epics-ca](https://github.com/physwkim/epics-base-rs) via the optional `epics` feature.

**Repository:** <https://github.com/epics-rs/epics-rs>

## Overview

asyn-rs provides the same driver model as C asyn, but uses Rust's type system and tokio for safety and concurrency:

- **PortDriver trait** — implement `read_*`/`write_*` for your hardware
- **ParamList** — named parameter cache with change tracking, timestamps, and alarm status
- **InterruptManager** — dual async (broadcast) + sync (mpsc) callback delivery
- **PortManager** — registry of named port drivers
- **AsynDeviceSupport** — bridges asyn-rs drivers to epics-ca `DeviceSupport` trait

## What's New in v0.2

### v0.2.0 — Actor Model + Typed Capabilities

**Actor-based port driver execution** — drivers are no longer accessed through `Arc<Mutex<dyn PortDriver>>`. Instead, each driver runs in its own actor thread with exclusive ownership:

- **PortActor** — owns the driver exclusively, dispatches requests via channel
- **PortHandle** — cloneable async handle with typed convenience methods (`read_int32()`, `write_float64()`, etc.)
- **AsyncCompletionHandle** — `Future` impl + `wait_blocking()` for sync callers

**Adapter migration** — `AsynDeviceSupport` now supports both legacy (`Arc<Mutex>`) and actor (`PortHandle`) backends via `PortBackend` enum. New drivers should use `from_handle()`.

**Typed capability system**:
- `InterfaceType` enum with bidirectional string conversion (e.g. `"asynInt32"` ↔ `InterfaceType::Int32`)
- `Capability` enum for declaring driver capabilities at type level
- `PortDriver::capabilities()` / `supports()` default trait methods

**Extended request types** — `RequestOp` extended with `DrvUserCreate`, `Enum`, `Int32Array`, `Float64Array`. `RequestResult` gains alarm/timestamp metadata.

### v0.2.1 — Protocol, Transport, Runtime

**Pure-data protocol** (`src/protocol/`) — serializable message types at all boundaries, no trait objects or closures:

| Type | Description |
|------|-------------|
| `PortCommand` | 23-variant enum, 1:1 map from `RequestOp` |
| `PortReply` | Response envelope with typed `ReplyPayload` |
| `ParamValue` | Serializable value union (no `GenericPointer`) |
| `PortRequest` | Request envelope with `RequestMeta` |
| `PortEvent` | Event with `EventPayload` (value change / exception) |

All types derive `serde::Serialize`/`Deserialize` for future wire transport.

**Pluggable transport** (`src/transport/`) — `RuntimeClient` trait decouples callers from transport:

```rust
pub trait RuntimeClient: Send + Sync + Clone + 'static {
    fn request(&self, req: PortRequest) -> Pin<Box<dyn Future<Output = Result<PortReply, TransportError>> + Send + '_>>;
    fn request_blocking(&self, req: PortRequest) -> Result<PortReply, TransportError>;
    fn subscribe(&self, filter: EventFilter) -> ...;
}
```

- **InProcessClient** — zero-cost fast path, direct enum pass-through (no serialization)
- Future: `UnixSocketClient` for multi-process deployments

**Runtime module** (`src/runtime/`) — promoted actors with lifecycle management:

- **PortRuntime** — promoted `PortActor` with `RuntimeEvent` broadcast (Started/Stopped/Connected/Disconnected/Error) and graceful shutdown
- **AxisRuntime** — per-axis motor actor with event emission, poll loop, and I/O Intr notification
- **Supervision** — generic restart loop with configurable policy (`max_restarts`, `restart_window`)
- **PortManager integration** — `register_port_runtime()` auto-registers both runtime handle and legacy port handle for backwards compatibility

**Criterion benchmarks** (`benches/throughput.rs`):
- `local_int32_read` / `local_float64_write` / `local_octet_roundtrip` — legacy mutex path
- `actor_int32_read` — PortHandle via actor
- `concurrent_32_producers` — 32 threads on same port
- `interrupt_event_throughput` — 1k events broadcast delivery

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
│  - InterfaceType / Capability declarations  │
│                                              │
│  PortDriverBase                              │
│  ├── ParamList (cache + change tracking)     │
│  ├── InterruptManager (broadcast + mpsc)     │
│  └── options: HashMap<String, String>        │
└─────────────────────────────────────────────┘
              ↕
┌─────────────────────────────────────────────┐
│  Your Hardware Driver                        │
│  - Background tokio task polls device        │
│  - set_*_param() + call_param_callbacks()    │
│  - Default read_* returns cached values      │
└─────────────────────────────────────────────┘
```

## Quick Start

Add to `Cargo.toml`:

```toml
[dependencies]
asyn-rs = { path = "../asyn-rs" }
# With EPICS integration:
# asyn-rs = { path = "../asyn-rs", features = ["epics"] }
```

### Implementing a Driver

```rust
use asyn_rs::param::ParamType;
use asyn_rs::port::{PortDriver, PortDriverBase, PortFlags};
use asyn_rs::error::AsynResult;

struct TemperatureDriver {
    base: PortDriverBase,
    temp_idx: usize,
}

impl TemperatureDriver {
    fn new() -> Self {
        let mut base = PortDriverBase::new("tempPort", 1, PortFlags::default());
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
use asyn_rs::manager::PortManager;

let manager = PortManager::new();
let port = manager.register_port(TemperatureDriver::new());

// Access from anywhere via Arc<RwLock<dyn PortDriver>>
let p = manager.find_port("tempPort").unwrap();
```

### EPICS Integration

With the `epics` feature, use `AsynDeviceSupport` to bridge drivers to epics-ca records:

```rust
use asyn_rs::adapter::{AsynDeviceSupport, parse_asyn_link};

// In a DeviceSupport factory:
let link = parse_asyn_link("@asyn(tempPort, 0, 1.0) TEMPERATURE").unwrap();
let port = manager.find_port(&link.port_name).unwrap();
let adapter = AsynDeviceSupport::new(port, link, "asynFloat64");
```

The adapter handles:
- Parameter resolution via `drvUserCreate`
- Value read/write through the port driver's cache
- Alarm status/severity propagation from driver to record
- Timestamp propagation (driver-supplied or auto-generated)
- I/O Intr scan support (broadcast → per-record mpsc bridge)

## Modules

| Module | Description |
|--------|-------------|
| `error` | `AsynStatus`, `AsynError` error types |
| `param` | `ParamList` — named parameter cache with types, change tracking, timestamps |
| `port` | `PortDriverBase` + `PortDriver` trait with cache-based I/O defaults |
| `interrupt` | `InterruptManager` — dual async/sync interrupt delivery |
| `manager` | `PortManager` — named port driver registry + runtime registration |
| `user` | `AsynUser` — per-request context (reason, addr) |
| `trace` | `asyn_trace!` macro for debug logging |
| `interfaces` | `InterfaceType`, `Capability` — typed interface/capability system |
| `port_actor` | `PortActor` — actor with exclusive driver ownership |
| `port_handle` | `PortHandle` — cloneable async handle with typed convenience methods |
| `protocol` | Pure-data message types: `PortCommand`, `PortReply`, `ParamValue`, `PortEvent` |
| `transport` | `RuntimeClient` trait, `InProcessClient` (zero-cost fast path) |
| `runtime` | `PortRuntime`, `AxisRuntime`, supervision, `RuntimeEvent` lifecycle |
| `adapter` | `AsynDeviceSupport` — epics-ca bridge *(requires `epics` feature)* |

## I/O Model

asyn-rs uses a **cache-based** model instead of C asyn's queue/block model:

1. A background task (e.g., tokio::spawn) polls the hardware
2. Driver calls `set_*_param()` to update cached values
3. Driver calls `call_param_callbacks()` to notify subscribers
4. Default `read_*` methods return the cached value immediately

This means `can_block` is preserved for compatibility but has no runtime effect. For command/response hardware, drivers manage their own async task and request queue.

## Testing

```bash
cargo test                    # Core tests (316)
cargo test --features epics   # With EPICS integration (326)
cargo bench                   # Criterion throughput benchmarks
```

## License

The Rust code authored in this crate is licensed under MIT.

This crate also bundles third-party OPI/UI assets related to EPICS asynDriver.
See [`THIRD_PARTY_LICENSES`](THIRD_PARTY_LICENSES) for attribution and upstream
license text.
