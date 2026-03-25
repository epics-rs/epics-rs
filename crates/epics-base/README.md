# epics-base-rs

Pure Rust implementation of [EPICS Base](https://epics-controls.org/) — Channel Access protocol (client & server), IOC runtime, and iocsh.

No C dependencies. No `libca`. Just `cargo build`.

**Repository:** <https://github.com/epics-rs/epics-rs>

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

# PV names with colons (EPICS convention)
softioc-rs --pv SEQ:counter:double:25.0 --pv SYS:status:string:OK

# With records (value is optional, defaults to 0)
softioc-rs --record ai:SENSOR:0.0 --record bo:SWITCH:1
softioc-rs --record ai:SEQ:counter --record bo:SEQ:light

# From a .db file
softioc-rs --db my_ioc.db -m "P=TEST:,R=TEMP"

# Custom port
softioc-rs --db my_ioc.db --port 5065
```

PV names can contain colons (e.g., `SEQ:counter`). For `--record`, the last `:` segment is used as the initial value only if it parses as the expected type; otherwise the entire remainder is treated as the PV name. For `--pv`, the type keyword (`double`, `string`, etc.) is detected automatically.

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
    channel.rs         # Channel abstraction (AccessRights, ChannelInfo)
    pva/               # pvAccess protocol (experimental)
    server/
      mod.rs           # CaServer, CaServerBuilder
      snapshot.rs      # Snapshot, AlarmInfo, DisplayInfo, ControlInfo, EnumInfo
      database.rs      # PvDatabase, link chain processing, CP link tracking
      record.rs        # Record trait, CommonFields, RecordInstance (shared infrastructure)
      tcp.rs           # TCP virtual circuit handler (per-client state, dispatch)
      udp.rs           # UDP search responder
      beacon.rs        # Beacon emitter (exponential backoff)
      scan.rs          # Periodic/event scan scheduler
      monitor.rs       # Subscription/monitor system (mpsc-based event delivery)
      calc_engine.rs   # Expression evaluator (CALC/CALCOUT fields)
      db_loader.rs     # .db file parser with macro substitution
      device_support.rs # DeviceSupport trait (init/read/write/I/O Intr)
      ioc_app.rs       # IocApplication (st.cmd lifecycle, 3-phase startup)
      iocsh/           # Interactive shell (C++ syntax tokenizer, commands)
      access_security.rs # ACF parser (UAG/HAG/ASG rules, per-channel enforcement)
      autosave.rs      # Save/restore (periodic save, atomic write, .req parsing)
      pv.rs            # ProcessVariable (simple PV, subscriber list)
      records/         # Per-record-type implementations (25 files)
  epics-macros/        # Proc macro for #[derive(EpicsRecord)]
```

### Channel Access Protocol

The CA protocol (`protocol.rs`, `types.rs`) provides the wire format for all client-server
communication. Every message starts with a 16-byte big-endian header:

| Field | Size | Description |
|-------|------|-------------|
| `cmmd` | u16 | Command code (message type) |
| `postsize` | u16 | Payload size (0xFFFF = extended) |
| `data_type` | u16 | DBR type code |
| `count` | u16 | Element count (0 = extended) |
| `cid` | u32 | Channel ID or status |
| `available` | u32 | Subscription ID or context |

When payload exceeds 64 KB or element count exceeds 65535, the extended header format is used:
`postsize=0xFFFF, count=0`, followed by 8 bytes of `extended_postsize` (u32) and
`extended_count` (u32). Maximum payload: 16 MB (DoS limit).

Key message types:

| Code | Name | Direction | Purpose |
|------|------|-----------|---------|
| 0 | VERSION | Both | Protocol version negotiation |
| 6 | SEARCH | C→S (UDP) | PV name lookup |
| 18 | CREATE_CHAN | C→S | Open channel, get CID |
| 15 | READ_NOTIFY | C→S | Read PV value |
| 19 | WRITE_NOTIFY | C→S | Write PV value (with ack) |
| 1 | EVENT_ADD | Both | Subscribe to changes / deliver updates |
| 2 | EVENT_CANCEL | C→S | Unsubscribe |
| 13 | RSRV_IS_UP | S→C (UDP) | Beacon announcement |
| 23 | ECHO | Both | Connection keepalive |
| 12 | CLEAR_CHANNEL | C→S | Close channel |

DBR types encode both the value type and metadata level:

| Range | Level | Contents |
|-------|-------|----------|
| 0–6 | PLAIN | Bare value |
| 7–13 | STS | + alarm status/severity |
| 14–20 | TIME | + EPICS timestamp |
| 21–27 | GR | + units, precision, display/alarm limits |
| 28–34 | CTRL | + control limits (DRVH/DRVL) |

Monitor masks control which changes trigger subscription updates:
`DBE_VALUE` (1), `DBE_LOG` (2), `DBE_ALARM` (4), `DBE_PROPERTY` (8).

### CA Client

The client (`client.rs`, `channel.rs`) implements the CA consumer side:

1. **Name resolution**: UDP broadcast `SEARCH` to `EPICS_CA_ADDR_LIST` (default: broadcast on port 5064)
2. **Connection**: TCP virtual circuit to the server's advertised port
3. **Channel creation**: `CREATE_CHAN` with PV name → server assigns CID + reports native type, element count, access rights
4. **I/O**: `READ_NOTIFY` / `WRITE_NOTIFY` for one-shot read/write; `EVENT_ADD` for subscriptions
5. **Cleanup**: `EVENT_CANCEL`, `CLEAR_CHANNEL`, TCP close

The `CaClient` API wraps this lifecycle:

```rust
let client = CaClient::new().await?;
let (dbr_type, value) = client.caget("TEMP").await?;  // search + connect + read
client.caput("TEMP", "42.0").await?;                   // write with callback
```

### CA Server

The server consists of three network components running as tokio tasks:

**UDP search responder** (`udp.rs`) — Listens on port 5064 (configurable via
`EPICS_CA_SERVER_PORT`). Parses incoming `SEARCH` messages, checks `PvDatabase::has_name()`,
and responds with `SEARCH` reply containing the server's TCP port.

**Beacon emitter** (`beacon.rs`) — Broadcasts `RSRV_IS_UP` on port 5065 with exponential
backoff (20 ms → 15 s, doubling each step). Clients use beacons to detect new servers without
re-searching.

**TCP virtual circuit handler** (`tcp.rs`) — Accepts connections and manages per-client state:

```rust
struct ClientState {
    channels: HashMap<u32, ChannelEntry>,        // CID → channel binding
    subscriptions: HashMap<u32, SubscriptionEntry>, // SUBID → active monitor
    channel_access: HashMap<u32, AccessLevel>,   // CID → computed ACF access
    hostname: String,                             // for ACF enforcement
    username: String,
}
```

The dispatch loop reads messages from the TCP stream and handles each command:
- `CREATE_CHAN` → look up PV, allocate CID, compute access rights, reply with type info
- `READ_NOTIFY` → build `Snapshot` from record/PV, encode to requested DBR type, reply
- `WRITE_NOTIFY` → decode payload, `put_field()` on record, trigger process + links, reply
- `EVENT_ADD` → create subscriber with mpsc channel, spawn monitor delivery task
- `EVENT_CANCEL` → drop subscriber, close channel
- `ECHO` → echo response (keepalive)

### PvDatabase and Link Chains

`PvDatabase` (`database.rs`) is the central registry for all PVs and records:

```rust
struct PvDatabase {
    simple_pvs: HashMap<String, ProcessVariable>,
    records: HashMap<String, RecordInstance>,
    scan_index: HashMap<ScanType, BTreeSet<(i32, String)>>,  // PHAS-sorted
    cp_links: HashMap<String, Vec<String>>,                   // source → targets
}
```

**PV name resolution** parses `"TEMP.EGU"` into record name `"TEMP"` + field `"EGU"` (default
field is `"VAL"`). Field values resolve through a 3-level priority: record-specific field →
common field (SEVR, STAT, SCAN, etc.) → VAL fallback.

**Link chain processing** handles forward links (FLNK) and channel-process links (CP):

- When a record is processed, `process_record_with_links()` follows FLNK to process
  downstream records in sequence, using a visited set to prevent cycles
- CP links are tracked in `cp_links`: when a source PV changes, all target records
  are automatically processed

**Timestamp source** (TSE field): TSE=0 uses system clock, TSE=-1 uses device-provided
time, TSE=-2 preserves the existing TIME field.

### Record System: `record.rs` vs `records/`

The record system is split into two layers:

**`record.rs`** contains shared infrastructure used by all record types. It is intentionally
a single file because these components are tightly coupled:

| Section | Lines | Contents |
|---------|-------|----------|
| Types & enums | 1–170 | `FieldDesc`, `AlarmSeverity`, `ScanType`, `AlarmStatus`, field metadata |
| `CommonFields` | 173–269 | Fields shared by every record: `VAL`, `NAME`, `DESC`, `SCAN`, `PINI`, alarm fields, etc. |
| Link parsing | 271–406 | `parse_link()`, CP/CPP/PP/MS modifiers, link chain resolution |
| `Record` trait | 408–524 | The trait all record types implement: `process()`, `read()`/`write()`, `special()`, `as_any_mut()`, `can_device_write()` |
| `RecordInstance` | 527–1377 | Core runtime (~850 lines): field get/put, alarm evaluation, process cycle, deadband, monitor subscriptions, snapshot generation |
| Tests | 1379–end | Unit tests for all of the above |

**`records/`** contains one file per record type (e.g., `ai.rs`, `ao.rs`, `bi.rs`, `motor.rs`, …).
Each file defines its type-specific fields and implements the `Record` trait. The
`#[derive(EpicsRecord)]` proc macro generates the boilerplate (field descriptors, get/put dispatch)
so that each record file focuses only on its unique processing logic.

This separation means adding a new record type never touches `record.rs` — you create a new file
in `records/`, derive the macro, and implement `Record`.

### Snapshot Model

`Snapshot` (`snapshot.rs`) is the single internal representation for PV state.
It carries everything needed to produce any DBR wire type:

```
Snapshot {
    value: EpicsValue,
    alarm: AlarmInfo { status, severity },
    timestamp: SystemTime,
    display: Option<DisplayInfo>,   // EGU, PREC, HOPR/LOPR, alarm limits
    control: Option<ControlInfo>,   // DRVH/DRVL
    enums: Option<EnumInfo>,        // ZNAM/ONAM or ZRST..FFST
}
```

`encode_dbr(dbr_type, &snapshot)` serializes a Snapshot to any of the 35 DBR wire formats.
All limits are stored internally as `f64` and cast to the wire-native type (i16/f32/i32/f64)
at serialization time.

### Device Support

The `DeviceSupport` trait (`device_support.rs`) connects records to external I/O drivers:

```rust
trait DeviceSupport: Send + Sync {
    fn init(&mut self, record: &mut dyn Record) -> CaResult<()>;
    fn read(&mut self, record: &mut dyn Record) -> CaResult<()>;
    fn write(&mut self, record: &mut dyn Record) -> CaResult<()>;
    fn dtyp(&self) -> &str;
    fn io_intr_receiver(&mut self) -> Option<mpsc::Receiver<()>>;
    fn write_begin(&mut self, record: &mut dyn Record)
        -> CaResult<Option<Box<dyn WriteCompletion>>>;
}
```

Records with a `DTYP` field delegate their I/O to a matching DeviceSupport instance:
- **init()**: Called once during iocInit (Phase 2). Can inject driver state into the record via `as_any_mut()` downcast.
- **read()**: Called during record process when the record has an input link to the driver.
- **write()**: Called when a CA client writes to the record (if `can_device_write()` returns true).
- **I/O Intr scanning**: `io_intr_receiver()` returns an `mpsc::Receiver<()>`. The scan system processes the record each time the driver sends a signal.
- **Async writes**: `write_begin()` submits the operation to a worker queue and returns a completion handle.

### Scan System

The scan scheduler (`scan.rs`) drives periodic and event-based record processing:

| Scan Type | Trigger |
|-----------|---------|
| Passive | Only processed via links (FLNK, CP) or CA writes |
| I/O Intr | Device support signals via mpsc channel |
| Event | External event injection via `submit_event()` |
| 10 Hz – 0.1 Hz | Periodic tokio tasks (100 ms – 10 s intervals) |

At IOC startup:
1. Records with `PINI=YES` are processed once (respecting PHAS ordering)
2. Periodic scan tasks are spawned (one tokio task per rate)
3. I/O Intr receivers are collected from device support and monitored

Periodic scans use the `scan_index` (BTreeSet sorted by PHAS) for deterministic ordering.
A visited set prevents infinite loops in link chain traversal.

### Monitor/Subscription System

The monitor system (`monitor.rs`, `pv.rs`) delivers value change notifications to CA clients:

```
Record process → put_field() → notify_subscribers()
                                      │
                        ┌─────────────┼─────────────┐
                        ▼             ▼             ▼
                  Subscriber 1   Subscriber 2   Subscriber N
                  (mpsc::tx)     (mpsc::tx)     (mpsc::tx)
                        │             │             │
                        ▼             ▼             ▼
                  monitor task   monitor task   monitor task
                  (encode+send)  (encode+send)  (encode+send)
```

Each `Subscriber` holds a mask (`DBE_VALUE | DBE_ALARM | ...`), requested DBR type, and an
mpsc sender. When a PV value changes, `notify_subscribers()` sends a `MonitorEvent` (containing
the full `Snapshot`) to each subscriber's channel. A dedicated tokio task per subscriber encodes
the snapshot and writes the `EVENT_ADD` response to the TCP connection. Closed subscribers are
automatically removed on the next notify cycle.

### IocApplication (st.cmd Lifecycle)

`IocApplication` (`ioc_app.rs`) provides the C EPICS-compatible IOC startup pattern:

| Phase | Thread | Action |
|-------|--------|--------|
| Phase 1 | std::thread | Execute st.cmd: `epicsEnvSet`, `dbLoadRecords`, driver config commands |
| Phase 2 | tokio | iocInit: wire device support → records, start scan tasks, start CA server |
| Phase 3 | std::thread | Interactive `epics>` REPL for runtime inspection |

Phase 1 runs on a blocking std::thread because iocsh commands use `Handle::block_on()` to call
async database methods synchronously. Phase 2 and the CA server run on the tokio runtime.
Phase 3 spawns another blocking thread for the interactive REPL.

Key builder methods:
- `register_startup_command(CommandDef)` — commands available during Phase 1
- `register_shell_command(CommandDef)` — commands available during Phase 3
- `register_device_support(dtyp, factory)` — static DTYP → factory mapping
- `register_dynamic_device_support(factory)` — chained fallback factory (new factory tries first, falls back to existing)
- `startup_script(path)` — path to st.cmd file

### iocsh (Interactive Shell)

The iocsh (`iocsh/`) provides a command-line interface with C++ EPICS-compatible syntax:

```
# C++ syntax (primary)
epicsEnvSet("PREFIX", "SIM1:")
dbLoadRecords("$(MY_DRIVER)/Db/sim.db", "P=$(PREFIX)")
myDriverConfig("SIM1", 256, 256)

# Macro substitution
$(VAR)          # environment variable
$(VAR=default)  # with default value
```

The tokenizer handles parentheses, comma-separated arguments, quoted strings, and
`$(MACRO)` expansion from environment variables. `CommandContext` provides the sync→async
bridge: `block_on()` runs futures from the REPL thread, `runtime_handle()` gives access
to the tokio handle for spawning tasks.

Built-in shell commands: `dbl` (list records), `dbgf` (get field), `dbpf` (put field),
`dbpr` (print record), `help`.

### Database Loader

The db loader (`db_loader.rs`) parses `.db` / `.template` files:

```
record(ao, "$(P)$(R)") {
    field(DESC, "Temperature")
    field(VAL,  "25.0")
    field(SCAN, "1 second")
    field(FLNK, "$(P)STATUS")
}
```

Parsing flow:
1. Read file, apply macro substitution (`$(NAME)`, `${NAME}`, `$(NAME=default)`)
2. Parse into `DbRecordDef` list (record type, name, fields)
3. For each definition, create record via built-in type map or `RecordFactory` registry
4. Apply fields via `put_field()` and common fields via `put_common_field()`
5. Two-phase init: `init_record(0)` then `init_record(1)`
6. Register in `PvDatabase`

External crates can register custom record types via `register_record_type()` to
override built-in stubs (e.g., asyn-rs registers `asynRecord`).

### Access Security

The ACF system (`access_security.rs`) provides per-channel read/write control:

```
UAG(admins) { alice, bob }
HAG(control_room) { cr-pc1, cr-pc2 }
ASG(RESTRICTED) {
    RULE(1, WRITE) { UAG(admins) HAG(control_room) }
    RULE(1, READ)
}
```

- **UAG** (User Access Group): named set of usernames
- **HAG** (Host Access Group): named set of hostnames
- **ASG** (Access Security Group): collection of rules; records reference an ASG via the `ASG` field

Access check: for each rule in the ASG, if the client's username matches a UAG member
and hostname matches a HAG member, grant the rule's level (Read or ReadWrite). No ACF
configured = all channels get full access.

### Autosave

The autosave system (`autosave.rs`) persists PV values across IOC restarts:

- **Save**: Periodically writes specified PVs to a file (one `PV_NAME value` per line),
  using atomic write (temp file + rename) for crash safety
- **Restore**: On IOC startup, reads the save file and calls `put_field()` for each entry
  before records are initialized
- **Request files**: `.req` format lists PVs to save, with `$(MACRO)` support
- **Backup**: Optional timestamped rolling backups

### CALC Engine

The expression evaluator (`calc_engine.rs`) powers `calc` and `calcout` records.
Supports the full C EPICS CALC syntax:

- **Variables**: A through L (12 inputs from INPA–INPL links)
- **Arithmetic**: `+`, `-`, `*`, `/`, `%`
- **Comparison**: `<`, `<=`, `>`, `>=`, `=`, `!=`, `#` (not equal)
- **Logic**: `&&`, `||`, `!`, `~` (bitwise NOT), `|`, `&`, `>>`, `<<`
- **Ternary**: `? :` (C-style conditional)
- **Math functions**: `ABS`, `SQR`, `SQRT`, `MIN`, `MAX`, `CEIL`, `FLOOR`,
  `LOG`, `LOGE`, `EXP`, `SIN`, `COS`, `TAN`, `ASIN`, `ACOS`, `ATAN`, `ATAN2`,
  `NINT`, `ISNAN`, `ISINF`, `FINITE`, `RANDOM`

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
