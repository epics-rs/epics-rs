# std-rs

Pure Rust port of the EPICS [std](https://github.com/epics-modules/std) synApps module — standard records and device support widely used in beamline IOCs at synchrotron and accelerator facilities.

No C dependencies. Just `cargo build`.

**Repository:** <https://github.com/epics-rs/epics-rs>

## Overview

The `std` module is a foundational synApps package providing PID feedback control, rate-limited outputs, formatted timestamps, sequence programs, and dozens of beamline utility databases. It's used as a base layer by virtually every synApps IOC.

std-rs ports the C synApps `std` module to native Rust, including the three core records (`epid`, `throttle`, `timestamp`), four device support implementations, native Rust async replacements for two SNL state machines (`delayDo`, `femto`), and 85 database templates ready to load with `dbLoadRecords`.

## Features

### Records

| Record | Description | Value |
|--------|-------------|-------|
| **epid** | Extended PID feedback controller (PID + MaxMin modes) | Double |
| **throttle** | Rate-limited output with drive limits and sync input | Double |
| **timestamp** | Wall-clock timestamp with format selection | String |

### epid Record
Extended PID controller with two modes:
- **PID mode** — full Kp/Ki/Kd PID with anti-windup, output deadband, bumpless turn-on, hysteresis-based alarms
- **MaxMin mode** — supervisory tracking that drives the output toward observed setpoint extremes
- Pre-process actions read INP (controlled value) and STPL (setpoint) links before each cycle
- `device_did_compute` flag lets device support replace the built-in PID with hardware-accelerated computation
- Output deadband (ODEL) prevents output oscillation
- Closed-loop / open-loop / supervisory modes via FBON

### throttle Record
Rate-limiting output forwarder:
- Reads input link (INP) and writes to output link (OUT)
- Rate limit RMAX (units/sec)
- Minimum delay RMIN between writes
- SYNC input for forced re-sync to current INP value
- Async delayed reprocess via `ReprocessAfter` action

### timestamp Record
Wall-clock timestamp string with 11 format options:
- ISO 8601, EPICS standard, custom strftime patterns
- Output to OUT link or read directly via VAL
- Periodic update via device support (Time of Day driver)

### Device Support

| Driver | Used For | Description |
|--------|----------|-------------|
| **Epid Soft** | epid | Synchronous PID computation each scan cycle |
| **Async Soft Channel** | epid | Trigger-driven async PID with callback completion |
| **Fast Epid** | epid | Interrupt-driven 1 kHz+ PID loop with high-rate readback |
| **Time of Day** | timestamp | Periodic wall-clock update via interval timer |
| **Sec Past Epoch** | longin | Unix epoch seconds counter |

### SNL Programs (Native Rust async)
The synApps `std` module ships with two SNL (State Notation Language) state machines that std-rs reimplements as native Rust async tasks:

- **femto** — Femto FEMTO amplifier gain control state machine. Manages cascaded transimpedance amplifier gain selection with auto-ranging based on signal level.
- **delayDo** — Generic delayed action state machine. Triggers a write after a configurable delay, with abort/restart support.

Both run as `tokio::spawn`'d tasks with `select!` for event/timeout multiplexing — no SNL compiler needed.

### Database Templates (85+ bundled in `db/`)
- **PID** — sync_pid_control, async_pid_control, fast_pid_control with autosave .req files
- **Femto** — DLPCA-200, DHPCA-100, DDPCA-300 amplifier presets
- **Generic state** — countDownTimer, alarmClock, autoShutter, genTweak, genericState (numeric/string/aux variants)
- **Sequence** — 4step, auto_4step (4-position state machines)
- **all_com** — common asyn record templates for serial port count 0/4/8/...88
- **delayDo** — generic delayed action sequencer

### Autosave Request Files
Each database template ships with a paired `.req` file for automatic save/restore via `epics-base-rs` autosave.

## Architecture

```
std-rs/
├── src/
│   ├── lib.rs                       # public API + std_record_factories()
│   ├── records/
│   │   ├── epid.rs                  # EpidRecord (PID + MaxMin)
│   │   ├── throttle.rs              # ThrottleRecord (rate-limited output)
│   │   └── timestamp.rs             # TimestampRecord (formatted wall clock)
│   ├── device_support/
│   │   ├── epid_soft.rs             # synchronous PID device support
│   │   ├── epid_soft_callback.rs    # async trigger-based variant
│   │   ├── epid_fast.rs             # 1 kHz+ interrupt-driven PID
│   │   └── time_of_day.rs           # periodic timestamp update
│   └── snl/
│       ├── delay_do.rs              # delayDo state machine
│       └── femto.rs                 # Femto amplifier control
├── db/                              # 85 database templates + .req files
└── ui/                              # PyDM screens
```

## Usage

```toml
[dependencies]
epics-rs = { version = "0.8", features = ["std"] }
```

### Register All Record Types

```rust
use epics_base_rs::server::ioc_app::IocApplication;
use epics_ca_rs::server::run_ca_ioc_app;
use std_rs::std_record_factories;

#[epics_base_rs::epics_main]
async fn main() -> epics_base_rs::error::CaResult<()> {
    let mut app = IocApplication::new();

    // Register epid, throttle, timestamp record types
    for (name, factory) in std_record_factories() {
        app = app.register_record_type(name, factory);
    }

    // `run_ca_ioc_app` runs C's `rsrvRegistrar` first, so `casr` and the
    // `dbsr` server layer exist before `iocInit` rather than after it.
    run_ca_ioc_app(app.db_file("db/sync_pid_control.db", &macros)?).await
}
```

### Load a PID Loop

```bash
# Set up a PID feedback loop
caput PID:KP 0.5
caput PID:KI 0.1
caput PID:KD 0.01
caput PID:VAL 25.0    # setpoint
caput PID:FBON 1      # enable feedback

# Watch the controlled variable converge
camonitor PID:CVAL PID:OVAL
```

## Testing

```bash
cargo test -p std-rs
```

Test coverage: PID computation correctness, anti-windup, output deadband, MaxMin tracking, throttle rate limiting, sync input behavior, timestamp format generation, delayDo state transitions, femto auto-ranging.

## Dependencies

- epics-base-rs — Record trait, DeviceSupport, ProcessAction
- asyn-rs — port driver (used by Femto cascaded amplifier)
- chrono — timestamp formatting
- epics-macros-rs — `#[derive(EpicsRecord)]`

## Requirements

- Rust 1.85+ (edition 2024)

## License

[EPICS Open License](../../LICENSE)

## See Also

- [EPICS std module](https://github.com/epics-modules/std) — original C synApps source
- [synApps documentation](https://epics-modules.github.io/) — beamline IOC reference
