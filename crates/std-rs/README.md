# std-rs

Pure Rust port of the EPICS [std](https://github.com/epics-modules/std) synApps module — standard records and device support widely used in beamline IOCs at synchrotron and accelerator facilities.

No C dependencies. Just `cargo build`.

**Repository:** <https://github.com/epics-rs/epics-rs>

## Overview

The `std` module is a foundational synApps package providing PID feedback control, rate-limited outputs, formatted timestamps, sequence programs, and dozens of beamline utility databases. It's used as a base layer by virtually every synApps IOC.

std-rs ports the C synApps `std` module (R3-6-4) to native Rust, including the three core records (`epid`, `throttle`, `timestamp`), five device supports, two SNL programs (`delayDo`, `femto`) compiled from the upstream `.st` by snc-rs and run on `epics-seq-rs`, and 51 database templates ready to load with `dbLoadRecords`, plus 31 autosave `.req` files.

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
- Feedback on/off via FBON, PID or Max/Min via FMOD

### throttle Record
Rate-limiting output forwarder:
- A put to VAL is checked against DRVLL/DRVLH (clipped when DRVLC is On) and written to the OUT link as SENT
- Minimum delay DLY between writes; a value put during it waits (WAIT) and the latest is sent when it ends
- SYNC reads the SINP link into VAL
- DLY cooldown via the `ValuePutAfter` action, never a process cycle

### timestamp Record
Wall-clock timestamp string in VAL, in one of 11 fixed formats chosen by TST
(`YY/MM/DD HH:MM:SS` through `MM/DD/YY HH:MM:SS.ns`). The record has no
output link and no device support.

### Device Support

| Driver | Used For | Description |
|--------|----------|-------------|
| **Soft Channel** | epid | Synchronous PID computation each scan cycle |
| **Async Soft Channel** | epid | Trigger-driven async PID with callback completion |
| **Fast Epid** | epid | Interrupt-driven 1 kHz+ PID loop with high-rate readback |
| **Time of Day** | stringin | the wall-clock string of `devTimeOfDay.c`'s `devSiTodString` |
| **Sec Past Epoch** | ai | seconds past the EPICS epoch (1990), `devTimeOfDay.c`'s `devAiTodSeconds` |

### SNL Programs
The synApps `std` module ships with two SNL (State Notation Language) programs. std-rs keeps the upstream `.st` in `src/snl/`, with C defects fixed in place under a comment; `build.rs` compiles them with epics-snc-rs and they run on epics-seq-rs:

- **femto** — Femto amplifier gain control. Sets the gain bits (G1–G3) and the noise bit (NO) of the selected gain.
- **delayDo** — Generic delayed action state machine. Triggers a write after a configurable delay, with abort/restart support.

As in C, each state set runs on its own OS thread. An IOC starts one with
`std_rs::seq_runner::seq_start(program, macros, bridge, db)`, or with
`epics_seq_rs::seq(&std_rs::snl::delay_do::delayDo, "P=xxx:,R=delayDo1", 0, &db)`.
No C SNL compiler is needed.

### Database Templates (51 `.db` and 3 `.vdb` in `db/`, with 31 `.req` files)
- **PID** — pid_control, async_pid_control, fast_pid_control with autosave .req files
- **Femto** — DLPCA-200, DHPCA-100, DDPCA-300 amplifier presets
- **Generic state** — countDownTimer, alarmClock, autoShutter, genTweak, genericState (numeric/string/aux variants)
- **Sequence** — 4step (4-position state machine)
- **all_com** — allstop/alldone records for 0 to 88 motors (`all_com_<N>.db`)
- **delayDo** — generic delayed action sequencer

### Autosave Request Files
31 `.req` files, most paired with a template, for save/restore via `epics-base-rs` autosave.

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
│   ├── records/dbd_generated.rs     # the field tables generated from the .dbd set
│   ├── seq_runner.rs                # the SNL program launcher, C's `seq()` stand-in
│   └── snl/
│       ├── delayDo.st, delay_do.rs  # upstream delayDo.st + its companion
│       └── femto.st, femto.rs       # upstream femto.st + its companion (powers table)
├── db/                              # 51 database templates + their .req files
└── ui/                              # PyDM screens
```

## Usage

```toml
[dependencies]
epics-rs = { version = "0.32", features = ["std"] }
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
    // The database loads through the shell, as in C: `db_file` is
    // `IocBuilder`'s, and an `IocApplication` has no second loader.
    run_ca_ioc_app(app.startup_line(&format!(
        r#"dbLoadRecords("{}/pid_control.db", "P=BL1:,PID=pid1,INP=BL1:temp,OUT=BL1:heater")"#,
        std_rs::STD_DB_DIR
    )))
    .await
}
```

### Load a PID Loop

```bash
# Set up a PID feedback loop
caput BL1:pid1.KP 0.5
caput BL1:pid1.KI 0.1
caput BL1:pid1.KD 0.01
caput BL1:pid1.VAL 25.0    # setpoint
caput BL1:pid1.FBON 1      # enable feedback

# Watch the controlled variable converge
camonitor BL1:pid1.CVAL BL1:pid1.OVAL
```

## Testing

```bash
cargo nextest run -p std-rs
```

Test coverage: PID computation correctness, anti-windup, output deadband, MaxMin tracking, throttle rate limiting, sync input behavior, timestamp format generation, delayDo and femto driven through their databases.

## Dependencies

- epics-base-rs — Record trait, DeviceSupport, ProcessAction (the three
  records' `Record` impls are hand-written, not derived)
- epics-ca-rs — the `run_ca_ioc_app` entry point the example uses
- asyn-rs — the Fast Epid device support's port I/O
- epics-seq-rs — the run time delayDo and femto run on; epics-snc-rs compiles them in `build.rs`
- chrono — timestamp formatting
- tokio, tracing

## Not ported

- The `pvHistory` aSub subroutine (`pvHistory.c`); `db/pvHistory.db` names it but nothing registers it.
- `doAfterIocInit` (`delayCmd.cpp`) and `iocCheck.c`.
- The VxWorks-only `devVXStats.c` and `vxcall.cpp`.

## Requirements

- Rust 1.99.1 (`rust-toolchain.toml`), edition 2024

## License

[EPICS Open License](../../LICENSE)

## See Also

- [EPICS std module](https://github.com/epics-modules/std) — original C synApps source
- [synApps documentation](https://epics-modules.github.io/) — beamline IOC reference
