# motor-rs

Pure Rust implementation of the [EPICS motor record](https://github.com/epics-modules/motor) — complete motor control with position tracking, velocity management, limit enforcement, backlash compensation, and motion state machine.

No C dependencies. Just `cargo build`.

**Repository:** <https://github.com/epics-rs/epics-rs>

## Features

- Full motor record processing with multi-phase motion state machine
- 9 motion phases: Idle, MainMove, BacklashFinal, Retry, Jog, JogStopping, JogBacklash, Homing, DelayWait
- Coordinate conversion: dial ↔ user position, dial ↔ raw steps
- Soft/hard limit enforcement; raw soft limits RHLM/RLLM invariant under MRES change
- Backlash compensation (approach + final move)
- Retry logic with configurable retry count and deadband (4 RetryModes: Default, Arithmetic, Geometric, InPosition)
- PID control fields
- Jog and homing support
- SPMG (Stop/Pause/Move/Go) command gate
- SET/OFF/DIR/FOFF mode support
- NTM retarget during motion
- UEIP readback
- Acceleration as ACCL (time) or ACCS (EGU/sec²) selected by ACCU; driver-bound rate is EGU/sec²
- RSTM restore mode (Never/Always/NearZero/Conditional) with MRES-mismatch interlock
- SYNC trigger, RVEL actual velocity, position-compare output (PCO)
- 64-bit raw fields (RVAL/RRBV/RMP/REP/RDIF) for high-resolution / long-travel axes
- Device support via asyn motor interface (move, home, moveToHome, stop, profile, PCO)
- Builder pattern for easy motor setup
- SimMotor for testing (time-based linear interpolation)

## What's New in v0.2

### v0.2.0 — Per-Axis Actor Runtime

**AxisRuntime** — unified per-axis actor that replaces the shared-state poll loop:

- **AxisRuntime** — single async task per axis, owns motor driver exclusively (no `Arc<Mutex>`)
- **AxisHandle** — cloneable async interface (`execute()`, `get_status()`, polling, delay)
- `select!` multiplexes commands and poll timer (no `SharedDeviceState` mutex)
- I/O Intr notification channel for scan integration

```rust
use motor_rs::axis_runtime::create_axis_runtime;

let (handle, _jh) = create_axis_runtime(sim_motor, Duration::from_millis(100));
handle.execute(actions).await;
let status = handle.get_status().await;
```

### v0.2.1 — Benchmarks

- **Criterion benchmark** (`benches/motor.rs`): `motor_move_to_done` measures full move cycle through AxisRuntime

## Architecture

```
motor-rs/
  src/
    lib.rs              # Public API
    record/             # MotorRecord — process(), state machine, field access
      mod.rs            #   Record trait impl, process()
      command_planner.rs#   plan_motion(), accel conversion, move emission
      state_machine.rs  #   check_completion(), phase transitions
      field_access.rs   #   FIELDS table, get/put handlers
      status_update.rs  #   determine_event(), process_motor_info(), RSTM init
    fields.rs           # Field groups: Position, Velocity, Limit, Control, PID, Pco, etc.
    flags.rs            # MipFlags, MstaFlags, MotionPhase, AccsUsed, RestoreMode
    coordinate.rs       # Dial ↔ user ↔ raw coordinate conversion (raw is i64)
    device_state.rs     # Shared mailbox between record, device support, poll loop
    device_support.rs   # MotorDeviceSupport — bridges record to AsynMotor drivers
    axis_runtime.rs     # AxisRuntime — per-axis actor with exclusive driver ownership
    poll_loop.rs        # Async polling task for motor status
    builder.rs          # MotorBuilder — fluent API for motor assembly
    sim_motor.rs        # SimMotor — simulated motor for testing
  benches/
    motor.rs            # Criterion benchmarks
  opi/
    medm/               # MEDM .adl screens (from C++ motor)
    pydm/               # PyDM .ui screens (converted via adl2pydm)
```

## Quick Start

```rust
use motor_rs::{MotorBuilder, SimMotor};

let sim = SimMotor::new();
let setup = MotorBuilder::new("MOTOR1", sim)
    .addr(0)
    .poll_interval(Duration::from_millis(100))
    .build();
```

## Testing

```bash
cargo nextest run   # 233 tests
cargo bench         # Criterion benchmarks
```

233 tests covering record processing, motion phases, coordinate conversion, device support, axis runtime, C parity (backlash, SET mode, retry, readback, NTM), and simulated motor behavior.

## Dependencies

- epics-base-rs — Record trait, DeviceSupport trait
- asyn-rs — AsynMotor interface, async runtime facade
- bitflags — MipFlags, MstaFlags

## Requirements

- Rust 1.85+ (edition 2024)

## License

[EPICS Open License](../../LICENSE)
