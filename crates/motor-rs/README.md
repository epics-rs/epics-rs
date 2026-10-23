# motor-rs

Pure Rust implementation of the [EPICS motor record](https://github.com/epics-modules/motor) (motor R7-4-5) — complete motor control with position tracking, velocity management, limit enforcement, backlash compensation, and motion state machine.

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
- Acceleration as ACCL (time) or ACCS (EGU/sec²) selected by ACCU; driver-bound rate is steps/sec² (EGU/sec² over |MRES|)
- RSTM restore mode (Never/Always/NearZero/Conditional) with MRES-mismatch interlock
- SYNC trigger, RVEL actual velocity
- Raw fields (RVAL/RRBV/RMP/REP/RDIF/RVEL/LRVL) are DBF_LONG as in motorRecord.dbd
- Device support via asyn motor interface (move, home, moveToHome, stop, profile)
- Model-1 drivers (`MotorBuilder::model1` on a `Model1Motor`, `register_model1_device`)
- The controller's own asyn port, as `asynMotorController`'s (`devices::register_controller_port`)
- Builder pattern for easy motor setup
- SimMotor: one motorSim axis on `route.c`'s path, stepped every 0.1 s

## Benchmarks

- **Criterion benchmark** (`benches/motor.rs`): `motor_move_to_done` measures
  a full record-level move cycle (VAL write → plan → completion).

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
    fields.rs           # Field groups: Position, Velocity, Limit, Control, PID, etc.
    flags.rs            # MipFlags, MstaFlags, MotionPhase, AccsUsed, RestoreMode
    coordinate.rs       # Dial ↔ user ↔ raw coordinate conversion (raw is i64)
    device_state.rs     # Shared mailbox between record, device support, poll loop
    device_support.rs   # MotorDeviceSupport — bridges record to AsynMotor drivers
    poll_loop.rs        # MotorPollLoop — the controller's motorPoller thread
    builder.rs          # MotorBuilder, ControllerBuilder — axis and poller assembly
    controller_port.rs  # the controller's asyn parameter port
    devices.rs          # the motor device() rows and the axes their DTYPs reach
    trace.rs            # the asyn user a motor's diagnostics print through
    sim_motor.rs        # SimMotor — one motorSim axis
    sim_motor/route.rs  #   motorSim's route.c path
    ioc.rs              # `simMotorCreate` and the dynamic device support
    record/dbd_generated.rs  # the field table generated from motorRecord.dbd
  benches/
    motor.rs            # Criterion benchmarks
  opi/
    medm/               # MEDM .adl screens (from C++ motor)
    pydm/               # PyDM .ui screens (converted via adl2pydm)
```

## Quick Start

```rust
use std::sync::{Arc, Mutex};
use std::time::Duration;

use asyn_rs::interfaces::motor::AsynMotor;
use asyn_rs::registry::open_owner;
use motor_rs::builder::{AxisSetup, MotorBuilder};
use motor_rs::devices::{register_axis, register_controller_port};
use motor_rs::sim_motor::SimMotor;
use motor_rs::trace::TraceUser;

register_controller_port(&open_owner(), "SIM", 1, ())?;
let motor: Arc<Mutex<dyn AsynMotor>> = Arc::new(Mutex::new(SimMotor::new()));
let AxisSetup { axis, poll_loop, .. } = MotorBuilder::new(motor)
    .trace_user(TraceUser::Asyn { port: "SIM".into(), addr: 0 })
    // one call sets both the moving and the idle interval; set them apart with
    // `moving_poll_interval` / `idle_poll_interval`
    .poll_interval(Duration::from_millis(100))
    .build_axis();
register_axis(axis)?;
// Starts the motorPoller thread.
poll_loop.start();
```

A record then reaches the axis with `DTYP "asynMotor"` and
`OUT "@asyn(SIM,0)"`; register `motor_rs::motor_record_factory()` and
`devices::devices()` / `devices::device_support_factory()` on the
`IocBuilder`.

## Testing

```bash
cargo nextest run -p motor-rs
cargo bench -p motor-rs
```

The suite covers record processing, motion phases, coordinate conversion,
device support, the poll loop, C parity (backlash, SET mode, retry, readback,
NTM), and simulated motor behaviour.

## Dependencies

- epics-base-rs — Record trait, DeviceSupport trait
- epics-ca-rs — the iocsh side of `simMotorCreate`
- epics-libcom-rs — `cfmt::CFloat`, C's float formatting
- asyn-rs — AsynMotor interface, async runtime facade
- bitflags — MipFlags, MstaFlags
- tokio

## Requirements

- Rust 1.99.1 (`rust-toolchain.toml`), edition 2024

## License

[EPICS Open License](../../LICENSE)
