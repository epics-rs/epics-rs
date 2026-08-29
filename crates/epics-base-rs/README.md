# epics-base-rs

Pure Rust implementation of the EPICS IOC core — record system, database, processing engine, iocsh, .db loader, access security, autosave, and calc engine.

No C dependencies. No `libCom`. Just `cargo build`.

**Repository:** <https://github.com/epics-rs/epics-rs>

## Overview

epics-base-rs is the foundation of the [epics-rs](https://github.com/epics-rs/epics-rs) workspace. It corresponds to the C EPICS Base modules `dbStatic`, `dbCommon`, `recCore`, `iocsh`, `asLib`, `autosave`, and `calc` — minus the wire protocol code (which lives in `epics-ca-rs` for Channel Access and `epics-pva-rs` for pvAccess).

```
┌──────────────────────────────────────────────┐
│              IocApplication                  │  ← high-level lifecycle
│  (st.cmd parser, device factories, builder)  │
└────────────────┬─────────────────────────────┘
                 │
         ┌───────▼────────┐
         │   PvDatabase   │  ← record storage + processing
         │ (Arc<RwLock>)  │
         └───────┬────────┘
                 │
    ┌────────────┼──────────────┐
    ▼            ▼              ▼
┌────────┐  ┌──────────┐  ┌──────────┐
│ Records│  │  Links   │  │ Subscr.  │
│ (trait)│  │ (parsed) │  │ (mpsc)   │
└────────┘  └──────────┘  └──────────┘
```

## Features

### Record System
- **Record trait** — `process()`, `get_field()`, `put_field()`, `field_list()`, `validate_put()`, `init_record()`, `special()`
- **#[derive(EpicsRecord)]** proc macro for boilerplate generation
- **CommonFields** — shared fields (NAME, RTYP, SCAN, PHAS, SEVR, STAT, TIME, DESC, etc.)
- **RecordInstance** — runtime wrapper with subscriber list, link state, processing flag, alarm evaluation
- **ProcessOutcome / ProcessAction** — pure state-machine records express side effects (link writes, delayed reprocess, device commands) as data
- **Snapshot** — bundled value + alarm + timestamp + display/control/enum metadata, assembled on demand

### Record Types (23+)
| Category | Types |
|----------|-------|
| Analog | ai, ao |
| Binary | bi, bo |
| Multi-bit binary | mbbi, mbbo |
| Long integer | longin, longout |
| String | stringin, stringout |
| Array | waveform, compress, histogram |
| Calculation | calc, calcout, scalcout, sub, asub |
| Selection | sel, seq, sseq, transform |
| Fanout | fanout, dfanout |
| Misc | busy, asyn |

### Database & Processing
- **PvDatabase** — Arc-shared record map with `add_record`, `get_record`, `process_record`, `process_record_with_links`, `put_record_field_from_ca`
- **Link parsing** — DB/CA/PVA/Constant links, INP/OUT/FLNK/SDIS/TSEL
- **Scan engine** — Passive, I/O Intr, Event, periodic (10/5/2/1/0.5/0.2/0.1 Hz), with PHAS ordering
- **Alarm propagation** — MS/NMS link maximize-severity, deadband filtering (MDEL/ADEL), state alarms (HIHI/HIGH/LOW/LOLO)
- **DBE event mask** — VALUE/LOG/ALARM/PROPERTY for fine-grained subscription
- **Origin tracking** — self-write filter for sequencer write-back loops

### Database Loader (.db files)
- **db_loader** — full `.db` parser: record/grecord/info, macro expansion `$(P)`/`${KEY=default}`, environment variable fallback, `include` directive
- **DbRecordDef** — field definitions, common field application via `put_common_field`
- **dbLoadRecords** — iocsh-compatible loader with `P=`, `R=` macro substitution

### IOC Lifecycle
- **IocBuilder** — programmatic IOC setup: `pv()`, `record()`, `db_file()`, `register_device_support()`, `register_record_type()`, `autosave()`
- **IocApplication** — st.cmd-style lifecycle (Phase 1: pre-init script, Phase 2: device wiring + autosave restore, Phase 3: protocol runner)
- **iocsh** — interactive shell with command registration, st.cmd parser, expression evaluator
- **Pluggable protocol runner** — CA, PVA, or both via `app.run(|config| async { ... })`

### Direct Database Access
- **DbChannel** — in-process get/put without wire protocol round-trip (`get_f64`, `put_f64_process`, `put_f64_post`)
- **DbSubscription** — real monitor via `add_subscriber`, returns `MonitorEvent` with full `Snapshot`
- **DbMultiMonitor** — wait on multiple PVs simultaneously
- **Origin filtering** — `subscribe_filtered(ignore_origin)` skips self-triggered events

### Access Security
- **ACF parser** — UAG (user groups), HAG (host groups), ASG (access security groups) with READ/WRITE/READWRITE permissions
- **PV-level enforcement** — checked on CA put operations
- **Per-instance overrides** — record `ASG` field links to ACF rule

### Calc Engine
- **Numeric calc** — infix-to-postfix compilation, 16 input variables (A–P), full math library (sqrt, sin, log, abs, floor, etc.)
- **String calc** — string concatenation, search, substring, format
- **Array calc** — element-wise operations, statistics (mean, sigma, min, max, median)

### Autosave
- C-compatible iocsh commands: `set_requestfile_path`, `set_savefile_path`, `create_monitor_set`, `create_triggered_set`, `set_pass0_restoreFile`, `set_pass1_restoreFile`, `save_restoreSet_status_prefix`, `fdbsave`, `fdbrestore`, `fdblist`
- Pass0 (before device support init) and Pass1 (after) restore stages
- `.req` file parsing with `file` includes, macro expansion, search path resolution, cycle detection
- Periodic / triggered / on-change / manual save strategies
- Atomic file write (tmp → fsync → rename), `.savB` backup rotation
- C autosave-compatible `.sav` file format

### Runtime Facade
- `epics_base_rs::runtime::sync` — `mpsc`, `Notify`, `RwLock`, `Mutex`, `Arc`
- `epics_base_rs::runtime::task` — `spawn`, `sleep`, `interval`, `timeout`
- `epics_base_rs::runtime::select` — async multiplexing
- `#[epics_base_rs::epics_main]` — IOC entry point (replaces `#[tokio::main]`)
- `#[epics_base_rs::epics_test]` — async test (replaces `#[tokio::test]`)

Driver authors should use this facade instead of depending on tokio directly.

### Real-time deployment (Linux PREEMPT_RT)

This crate owns the two real-time levers a Linux RT deployment uses:

- **`--features linux-rt`** — wires `runtime::sync::PriorityInheritanceMutex`
  to a `pthread_mutex_t` configured with `PTHREAD_PRIO_INHERIT`, so a
  low-priority lock holder inherits its waiter's priority instead of inverting
  it. Off by default (the default is a `parking_lot` fallback with no PI);
  only meaningful on systems actually running SCHED_FIFO/SCHED_RR. No-op on
  non-Linux.
- **`EPICS_RS_ALLOW_RT_PRIORITY=YES`** — opts the process into best-effort
  SCHED_FIFO banding for IOC threads (falls back to default scheduling when
  the OS refuses). The default is `NO` on hosted targets and `YES` on RTEMS;
  an explicit value wins in both directions. Granting the privilege on Linux
  is the operator's side: an `RTPRIO` rlimit, `CAP_SYS_NICE`, or launching
  under `chrt`.

`EPICS_RS_BUILD_EXEC_BACKEND=thread` is the matching execution-model switch
for a blocking-front-end deployment (dedicated OS threads, no tokio reactor) —
`crates/epics-libcom-rs/build.rs` states the intended Linux PREEMPT_RT use, and
why the switch is an environment variable rather than a cargo feature.

Measured on a PREEMPT_RT kernel in `doc/rtlinux-rt-measurement.md`: with PI on,
the record-gate priority inversion the mutex was built to kill collapses to the
critical-section bound (worst case 24.9 ms → 10.1 ms on the measured guest;
absolute numbers are VM-relative, the ratios are the defensible part). The scan
latency decomposition lives in `doc/rtlinux-scan-tail-decomposition.md`.

```
epics-base-rs/src/
├── lib.rs
├── error.rs                # CaError, CaResult
├── runtime/                # async runtime facade (mpsc, Notify, spawn, select)
├── types/
│   ├── value.rs            # EpicsValue (12 variants: scalar + array)
│   ├── dbr.rs              # DbFieldType, DBR type ranges
│   └── codec.rs            # DBR encoding/decoding (PLAIN/STS/TIME/GR/CTRL)
├── calc/                   # expression engine (numeric/string/array)
└── server/
    ├── ioc_app.rs          # IocApplication (high-level lifecycle)
    ├── ioc_builder.rs      # IocBuilder (programmatic setup)
    ├── iocsh/              # interactive shell + st.cmd parser
    ├── database/
    │   ├── mod.rs          # PvDatabase + parse_pv_name
    │   ├── field_io.rs     # get_pv, put_pv, put_record_field_from_ca
    │   ├── processing.rs   # process_record_with_links (full link chain)
    │   ├── links.rs        # DB/CA/PVA/Constant link resolution
    │   ├── scan_index.rs   # SCAN scheduling (Passive/Periodic/IOIntr/Event)
    │   └── db_access.rs    # DbChannel, DbSubscription, DbMultiMonitor
    ├── db_loader/          # .db parser, macro expansion, info()
    ├── record/
    │   ├── record_trait.rs # Record trait, FieldDesc, ProcessOutcome
    │   └── record_instance.rs  # CommonFields, snapshot_for_field, alarm eval
    ├── records/            # 23 record type implementations
    ├── snapshot.rs         # Snapshot, AlarmInfo, DisplayInfo, ControlInfo, EnumInfo
    ├── pv.rs               # ProcessVariable, MonitorEvent, Subscriber
    ├── recgbl.rs           # EventMask (VALUE/LOG/ALARM/PROPERTY)
    ├── scan.rs             # ScanType
    ├── scan_event.rs       # event-driven scanning
    ├── access_security.rs  # ACF parser + UAG/HAG/ASG
    ├── device_support.rs   # DeviceSupport trait, DeviceSupportFactory
    └── autosave/           # save/restore (Pass0/Pass1, request files)
```

## Quick Start

```rust
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::records::ai::AiRecord;
use epics_base_rs::types::EpicsValue;

#[epics_base_rs::epics_main]
async fn main() -> epics_base_rs::error::CaResult<()> {
    let (db, _autosave) = IocBuilder::new()
        .pv("MSG", EpicsValue::String("hello".into()))
        .record("TEMP", AiRecord::new())
        .build()
        .await?;

    // db is Arc<PvDatabase> — pass to a protocol runner
    Ok(())
}
```

### Direct Database Access (no CA)

```rust
use epics_base_rs::server::database::db_access::{DbChannel, DbSubscription};

let ch = DbChannel::new(&db, "TEMP");
ch.put_f64_process(25.0).await?;
let v = ch.get_f64().await;

let mut sub = DbSubscription::subscribe(&db, "TEMP").await.unwrap();
while let Some(snap) = sub.recv_snapshot().await {
    println!("{:?}", snap.value);
}
```

### Custom Record Type

```rust
use epics_base_rs::server::record::Record;
use epics_macros_rs::EpicsRecord;

#[derive(EpicsRecord, Default)]
#[record(type = "myrec")]
pub struct MyRecord {
    #[field(type = "Double")]
    pub val: f64,
    #[field(type = "String")]
    pub desc: String,
}

impl Record for MyRecord {
    fn process(&mut self) -> CaResult<ProcessOutcome> {
        // your logic
        Ok(ProcessOutcome::complete())
    }
    // ... rest auto-generated by derive
}
```

## Testing

```bash
cargo test -p epics-base-rs
```

Test coverage: record processing, alarm evaluation, deadband filtering, link chain execution, scan scheduling, db file parsing, macro expansion, calc engine (numeric/string/array), DBR encoding (golden packets), access security, autosave save/restore, iocsh command registration.

## Dependencies

- chrono — timestamp formatting
- bytes — buffer management
- thiserror — error types
- tokio — async runtime (re-exported via `runtime::` facade)

## Requirements

- Rust 1.85+ (edition 2024)

## License

[EPICS Open License](../../LICENSE)
