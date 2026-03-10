# epics-base-rs

Pure Rust implementation of [EPICS Base](https://epics-controls.org/) — Channel Access protocol (client & server), IOC runtime, and iocsh.

No C dependencies. No `libca`. Just `cargo build`.

All binaries use the `-rs` suffix (e.g. `caget-rs`, `caput-rs`, `softioc-rs`) to avoid conflicts with the C EPICS tools.

## Features

### Client

- `caget-rs` — read a PV value
- `caput-rs` — write a PV value
- `camonitor-rs` — subscribe to PV changes
- `cainfo-rs` — display PV metadata
- UDP name resolution and TCP virtual circuit
- Extended header support for payloads > 64 KB

### Server (Soft IOC)

- `softioc-rs` binary — host PVs over Channel Access
- **20 record types**: ai, ao, bi, bo, stringin, stringout, longin, longout, mbbi, mbbo, waveform, calc, calcout, fanout, dfanout, seq, sel, compress, histogram, sub
- `.db` file loader with macro substitution
- Record processing with full link chain traversal (INP → process → OUT → FLNK)
- Periodic and event scan scheduling (Passive, Event, 10 Hz, 5 Hz, 2 Hz, 1 Hz, 0.5 Hz, 0.2 Hz, 0.1 Hz)
- CALC engine (arithmetic, comparison, logic, ternary, math functions)
- Monitor subscriptions with per-channel event delivery
- Beacon emitter
- Access security (ACF file parser, UAG/HAG/ASG rules, per-channel enforcement)
- Autosave/restore (periodic save, atomic write, type-aware restore)
- Device support trait for custom I/O drivers
- Subroutine registry for sub records
- **IocApplication**: st.cmd-style IOC lifecycle (C++ EPICS-compatible syntax)
- **iocsh**: Interactive shell with C++ EPICS function-call syntax and `$(VAR)` substitution
- Extended CA header for large payloads (> 64 KB) with 16 MB DoS limit

### pvAccess (experimental)

- `pvaget-rs`, `pvaput-rs`, `pvamonitor-rs`, `pvainfo-rs` — basic pvAccess client tools

## What's New in v0.2

### v0.2.0 — Declarative IOC Builder + Event Scheduler

**Declarative IOC configuration** — build IOCs entirely in Rust without `.db` files:

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

**ScanSchedulerV2** — event-driven scan scheduling with coalescing:
- `ScanEventKind`: Periodic, IoIntr, Event, Delayed, Pini
- `submit_event()` / `submit_delayed()` for external event injection
- HashSet-based dedup within scan ticks — no duplicate processing

## What's New in v0.3

### v0.3.0 — Snapshot-Based Internal Model (GR/CTRL Metadata)

**The problem**: `caget -d DBR_CTRL_DOUBLE <pv>` returned zeroed units/limits/precision because GR/CTRL DBR types (21-34) were serialized identically to TIME — no metadata was populated.

**The fix**: A new `Snapshot` type serves as the single internal state representation, carrying value + alarm + timestamp + display/control/enum metadata. The CA serializer now encodes real metadata into GR/CTRL wire frames.

```
┌──────────────────────────────────────────────────────────┐
│                     Snapshot                             │
│  value: EpicsValue                                       │
│  alarm: AlarmInfo { status, severity }                   │
│  timestamp: SystemTime                                   │
│  display: Option<DisplayInfo>  ← EGU, PREC, HOPR/LOPR   │
│  control: Option<ControlInfo>  ← DRVH/DRVL               │
│  enums:   Option<EnumInfo>     ← ZNAM/ONAM, ZRST..FFST   │
└──────────────────────────────────────────────────────────┘
        │
        ▼  encode_dbr(dbr_type, &snapshot)
┌──────────────────────────────────────────────────────────┐
│  DBR_PLAIN (0-6)   → bare value bytes                    │
│  DBR_STS   (7-13)  → status + severity + value           │
│  DBR_TIME  (14-20) → status + severity + timestamp + val │
│  DBR_GR    (21-27) → sts + units + prec + limits + val   │  ← NEW: real data
│  DBR_CTRL  (28-34) → sts + units + prec + limits + ctrl  │  ← NEW: real data
│                       limits + val                        │
└──────────────────────────────────────────────────────────┘
```

**Metadata populated per record type:**

| Record type | DisplayInfo | ControlInfo | EnumInfo |
|-------------|-------------|-------------|----------|
| ai | EGU, PREC, HOPR/LOPR, alarm limits | HOPR/LOPR | — |
| ao | EGU, PREC, HOPR/LOPR, alarm limits | DRVH/DRVL | — |
| longin/longout | EGU, HOPR/LOPR, alarm limits | HOPR/LOPR or DRVH/DRVL | — |
| bi/bo | — | — | ZNAM, ONAM |
| mbbi/mbbo | — | — | ZRST..FFST (16 strings) |

**Before (v0.2)**:
```
$ caget -d DBR_CTRL_DOUBLE TEMP
    Units:
    Precision:    0
    Upper limit:  0
    Lower limit:  0
```

**After (v0.3)**:
```
$ caget -d DBR_CTRL_DOUBLE TEMP
    Units:        degC
    Precision:    3
    Upper limit:  100
    Lower limit:  -50
```

### Differences from C EPICS

The Snapshot model departs from C EPICS internals while preserving wire compatibility:

| Aspect | C EPICS | epics-base-rs |
|--------|---------|---------------|
| **Internal state** | Metadata baked into each `dbCommon` / record struct as C struct fields; accessed via `dbAddr` pointer arithmetic | Metadata assembled on-demand into a `Snapshot` from `Record` trait + `CommonFields`; no pointer arithmetic |
| **GR/CTRL serialization** | `db_access.c` reads fields directly from the record's memory via `dbAddr` offsets into the flat C struct | `encode_dbr()` reads from `Snapshot.display` / `Snapshot.control` / `Snapshot.enums`; the serializer casts f64 → wire-native type (i16/f32/i32/f64) |
| **Limit storage** | Native type per record (e.g., `dbr_ctrl_short` stores `dbr_short_t` limits) | All limits stored as `f64` internally; cast to wire type at serialization time — matches the pattern used by C `dbFastLinkConv` |
| **Enum strings** | Fixed `char[MAX_ENUM_STATES][MAX_ENUM_STRING_SIZE]` in the record struct | `Vec<String>` in `EnumInfo`; padded to 16×26-byte slots at encoding time |
| **Timestamp** | `epicsTimeStamp` (EPICS epoch, 2×u32) stored in record | `SystemTime` stored in `CommonFields`; EPICS epoch conversion in serializer |
| **Display precision** | `PREC` field as `epicsInt16` in individual record structs | `precision: i16` inside `DisplayInfo`; only present for numeric record types |
| **Per-request allocation** | Zero allocation — `db_access.c` fills a pre-sized buffer from record pointers | `Snapshot` allocates `String` for EGU and `Vec<String>` for enums per request (<1μs; monitor path can cache) |
| **Field dispatch** | `dbAccess.c` switch on `dbAddr->field_type` + `dbAddr->no_elements` | `RecordInstance::snapshot_for_field()` matches on `record_type()` string |

**Wire compatibility**: The exact same bytes appear on the wire as a C EPICS server would produce. A C client (`caget`, `camonitor`, `cainfo`) sees no difference.

## Quick Start

```bash
cargo build --release
```

### Run a Soft IOC

```bash
# Simple PVs
softioc-rs --pv TEMP:double:25.0 --pv MSG:string:hello

# With records
softioc-rs --record ai:SENSOR:0.0 --record bo:SWITCH:0

# From a .db file
softioc-rs --db my_ioc.db -m "P=TEST:,R=TEMP"

# Custom port
softioc-rs --db my_ioc.db --port 5065
```

### Client Tools

```bash
# Read
caget-rs TEMP

# Write
caput-rs TEMP 42.0

# Monitor
camonitor-rs TEMP
```

### Library Usage

#### Embedded Server

```rust
use epics_base_rs::server::CaServer;
use epics_base_rs::server::records::ao::AoRecord;

#[tokio::main]
async fn main() -> epics_base_rs::error::CaResult<()> {
    let server = CaServer::builder()
        .record("TEMP", AoRecord::new(25.0))
        .record("SETPOINT", AoRecord::new(0.0))
        .build()
        .await?;

    server.run().await
}
```

#### With .db File and Access Security

```rust
use std::collections::HashMap;
use epics_base_rs::server::CaServer;

#[tokio::main]
async fn main() -> epics_base_rs::error::CaResult<()> {
    let macros = HashMap::from([
        ("P".into(), "MY:".into()),
        ("R".into(), "TEMP".into()),
    ]);

    let server = CaServer::builder()
        .db_file("ioc.db", &macros)?
        .acf_file("security.acf")?
        .build()
        .await?;

    server.run().await
}
```

#### With Autosave

```rust
use std::path::PathBuf;
use std::time::Duration;
use epics_base_rs::server::CaServer;
use epics_base_rs::server::autosave::AutosaveConfig;
use epics_base_rs::server::records::ao::AoRecord;

#[tokio::main]
async fn main() -> epics_base_rs::error::CaResult<()> {
    let server = CaServer::builder()
        .record("TEMP", AoRecord::new(25.0))
        .autosave(AutosaveConfig {
            save_path: PathBuf::from("/tmp/ioc.sav"),
            period: Duration::from_secs(30),
            request_pvs: vec!["TEMP".into()],
        })
        .build()
        .await?;

    server.run().await
}
```

#### IocApplication (st.cmd-style IOC)

For driver developers building IOCs with custom device support — matching the C++ EPICS `st.cmd` startup pattern:

```rust
use epics_base_rs::server::ioc_app::IocApplication;
use epics_base_rs::server::iocsh::registry::*;

#[tokio::main]
async fn main() -> epics_base_rs::error::CaResult<()> {
    IocApplication::new()
        .register_startup_command(/* driver config command */)
        .register_device_support("myDriver", || Box::new(MyDeviceSupport::new()))
        .register_shell_command(/* runtime commands */)
        .startup_script("ioc/st.cmd")
        .run()
        .await
}
```

The startup script uses **exact C++ EPICS syntax**:

```bash
# ioc/st.cmd — identical to C++ EPICS
epicsEnvSet("PREFIX", "SIM1:")
epicsEnvSet("CAM",    "cam1:")
myDriverConfig("SIM1", 256, 256, 50000000)
dbLoadRecords("$(MY_DRIVER)/Db/myDriver.db", "P=$(PREFIX),R=$(CAM)")
iocInit()
```

**IOC lifecycle** (matches C++ EPICS):

| Phase | Action | C++ Equivalent |
|-------|--------|----------------|
| Phase 1 | Execute `st.cmd` (epicsEnvSet, dbLoadRecords, driver config) | st.cmd before `iocInit()` |
| Phase 2 | Wire device support, restore autosave, start scan tasks | `iocInit()` |
| Phase 3 | Interactive `epics>` shell (dbl, dbgf, dbpf, dbpr, ...) | iocsh REPL |

## IOC Module System (C++ `.dbd` → Rust Crates)

In C++ EPICS, `.dbd` files and `Makefile` control which modules (record types, device support, drivers) are included in an IOC. In Rust, this maps to Cargo's crate and feature system:

| C++ EPICS | Rust Equivalent |
|-----------|-----------------|
| `.dbd` files (module declarations) | `Cargo.toml` `[dependencies]` |
| `Makefile` `xxx_DBD +=` (add/remove modules) | Add/remove crate dependencies |
| `envPaths` (build-time path generation) | `DB_DIR` const via `CARGO_MANIFEST_DIR` |
| `< envPaths` in st.cmd | IOC binary `set_var()` at startup |
| `$(ADSIMDETECTOR)/db/file.template` | `$(SIM_DETECTOR)/Db/file.db` |
| `registrar()` / `device()` in `.dbd` | `register_device_support()` call |
| `#ifdef` conditional include | Cargo `features` |

### Project Structure Convention

Each driver crate follows the C++ EPICS layout:

```
my-driver/
  src/                  ← xxxApp/src/    (driver source)
    lib.rs
    driver.rs
    bin/my_ioc.rs       ← IOC binary
  Db/                   ← xxxApp/Db/     (database templates, ship with driver)
    myDriver.db
  ioc/                  ← iocBoot/iocXxx/ (deployment-specific startup)
    st.cmd
```

- **`Db/`** — database templates that belong to the driver (reusable across IOCs)
- **`ioc/`** — deployment-specific st.cmd and configuration
- **`src/`** — driver source code and IOC binary

### Database Path Resolution

Each driver crate exports a `DB_DIR` constant with its absolute `Db/` path:

```rust
// In driver crate's lib.rs
pub const DB_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/Db");
```

The IOC binary sets environment variables at startup (like C++ `envPaths`):

```rust
// In IOC binary main()
unsafe { std::env::set_var("SIM_DETECTOR", sim_detector::DB_DIR.trim_end_matches("/Db")); }
```

Then `st.cmd` uses standard `$(MODULE)` references:

```bash
dbLoadRecords("$(SIM_DETECTOR)/Db/simDetector.db", "P=$(PREFIX),R=$(CAM)")
```

#### Client

```rust
use epics_base_rs::client::CaClient;

#[tokio::main]
async fn main() {
    let client = CaClient::new().await.unwrap();

    // Read
    let (_type, value) = client.caget("TEMP").await.unwrap();
    println!("TEMP = {value}");

    // Write
    client.caput("TEMP", "42.0").await.unwrap();
}
```

## .db File Format

Standard EPICS database format with macro substitution:

```
record(ao, "$(P)$(R)") {
    field(DESC, "Temperature setpoint")
    field(VAL,  "25.0")
    field(HOPR, "100.0")
    field(LOPR, "0.0")
    field(HIHI, "90.0")
    field(HIGH, "70.0")
    field(LOW,  "5.0")
    field(LOLO, "2.0")
    field(HHSV, "MAJOR")
    field(HSV,  "MINOR")
    field(LSV,  "MINOR")
    field(LLSV, "MAJOR")
    field(SCAN, "1 second")
    field(INP,  "SIM:RAW")
    field(FLNK, "$(P)$(R):STATUS")
}
```

## Access Security (ACF)

```
UAG(admins) { alice, bob }
HAG(control_room) { cr-pc1, cr-pc2 }

ASG(DEFAULT) {
    RULE(1, WRITE)
    RULE(1, READ)
}

ASG(RESTRICTED) {
    RULE(1, WRITE) { UAG(admins) HAG(control_room) }
    RULE(1, READ)
}
```

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
| calcout | Calculation with output | Double (OVAL) |
| fanout | Forward link fanout | — |
| dfanout | Data fanout | Double |
| seq | Sequence | Double |
| sel | Select | Double |
| compress | Circular buffer / N-to-1 compression | DoubleArray |
| histogram | Signal histogram | LongArray |
| sub | Subroutine | Double |

## Architecture

```
epics-base-rs/
  src/
    client.rs          # CA client (caget-rs/caput-rs/camonitor-rs)
    protocol.rs        # CA protocol codec (normal + extended header)
    types.rs           # EpicsValue, DbFieldType, serialize_dbr, encode_dbr
    error.rs           # Error types
    channel.rs         # Channel abstraction
    pva/               # pvAccess protocol (experimental)
    server/
      mod.rs           # CaServer, CaServerBuilder
      snapshot.rs      # Snapshot, AlarmInfo, DisplayInfo, ControlInfo, EnumInfo
      database.rs      # PvDatabase, link chain processing
      record.rs        # Record trait, CommonFields, RecordInstance, snapshot_for_field
      tcp.rs           # TCP virtual circuit handler (READ_NOTIFY → encode_dbr)
      udp.rs           # UDP search responder
      beacon.rs        # Beacon emitter
      scan.rs          # Periodic scan scheduler
      monitor.rs       # Subscription/monitor system
      calc_engine.rs   # Expression evaluator
      db_loader.rs     # .db file parser
      device_support.rs # DeviceSupport trait
      ioc_app.rs       # IocApplication (st.cmd lifecycle)
      iocsh/           # Interactive shell (C++ syntax tokenizer, commands)
      access_security.rs # ACF parser and access checks
      autosave.rs      # Save/restore
      pv.rs            # ProcessVariable (simple PV, snapshot)
      records/         # 20 record type implementations
  epics-macros/        # Proc macro for #[derive(EpicsRecord)]
```

### Data Flow: CA Read Request

```
Client                    tcp.rs                     record.rs / pv.rs          types.rs
  │                         │                             │                        │
  │  CA_PROTO_READ_NOTIFY   │                             │                        │
  │ (dbr_type=DBR_CTRL_DOUBLE)                            │                        │
  │────────────────────────>│                             │                        │
  │                         │  get_full_snapshot()        │                        │
  │                         │────────────────────────────>│                        │
  │                         │                             │  snapshot_for_field()  │
  │                         │                             │  ┌─ resolve_field()    │
  │                         │                             │  ├─ populate_display() │
  │                         │                             │  ├─ populate_control() │
  │                         │                             │  └─ populate_enum()    │
  │                         │       Snapshot              │                        │
  │                         │<────────────────────────────│                        │
  │                         │                             │                        │
  │                         │  encode_dbr(34, &snapshot)  │                        │
  │                         │────────────────────────────────────────────────────>│
  │                         │                             │   encode_ctrl()        │
  │                         │                             │   ┌─ status/severity   │
  │                         │                             │   ├─ prec + units      │
  │                         │       wire bytes            │   ├─ 8 limits (f64)    │
  │                         │<────────────────────────────────────────────────────│
  │    CA response          │                             │   └─ value             │
  │<────────────────────────│                             │                        │
```

## Testing

```bash
cargo test
```

286 tests covering protocol encoding, wire format golden packets, snapshot generation, GR/CTRL metadata serialization, record processing, link chains, calc engine, .db parsing, access security, autosave, iocsh, declarative IOC builder, and event scheduling.

## Requirements

- Rust 1.70+
- tokio runtime

## License

MIT
