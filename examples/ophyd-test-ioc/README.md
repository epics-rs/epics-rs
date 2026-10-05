# Ophyd Test IOC

Provides the PVs expected by [ophyd](https://github.com/bluesky/ophyd)'s test suite, replacing the Docker-based [epics-services-for-ophyd](https://github.com/bluesky/epics-services-for-ophyd).

## Devices

### Motors

9 `SimMotor` instances matching ophyd test expectations:

| PV | Description |
|----|-------------|
| `XF:31IDA-OP{Tbl-Ax:X1}Mtr` .. `X6}Mtr` | 6 table-axis motors |
| `sim:mtr1`, `sim:mtr2` | sim-prefix motors |
| `XF:31IDA-OP{Tbl-Ax:FakeMtr}` | Fake motor for fallback tests |

### Sensors

6 Soft Channel sensors (`1 second` scan):

| PV | Description |
|----|-------------|
| `XF:31IDA-BI{Dev:1}E-I` .. `{Dev:6}E-I` | Simulated current sensors |

### Area Detector (MovingDot)

A MovingDot 2D area detector using the ADDriver pattern with standard areaDetector plugins.

| PV | Description |
|----|-------------|
| `XF:31IDA-BI{Cam:Tbl}:cam1:Acquire` | Start/stop acquisition |
| `XF:31IDA-BI{Cam:Tbl}:cam1:AcquireTime` | Exposure time (s) |
| `XF:31IDA-BI{Cam:Tbl}:cam1:ImageMode` | Single / Multiple / Continuous |
| `XF:31IDA-BI{Cam:Tbl}:image1:ArrayData` | Image data waveform |

Plugins are loaded twice — under both `XF:31IDA-BI{Cam:Tbl}:` and `ADSIM:` prefixes for ophyd test compatibility.

The MovingDot acquisition task is fully async. A driver author only needs three types from `ad_core_rs::plugin::channel`:

| Type | Purpose |
|------|---------|
| `PortHandle` | Read/write parameters via `read_int32().await`, `set_params_and_notify().await` |
| `ArrayPublisher` | Publish a generated frame to downstream plugins: `publisher.publish(frame).await` |
| `QueuedArrayCounter` | Wait until in-flight frames drain at end of acquisition |

The acquisition loop runs inside a `current_thread` tokio runtime created by the task thread. All I/O is async and reliable — there are no lossy or blocking APIs in the data path.

## Build and Run

```bash
cargo build --release -p ophyd-test-ioc --features ioc
./target/release/ophyd-test-ioc examples/ophyd-test-ioc/ioc/st.cmd
```

## Architecture

All records use standard asyn DTYPs (`asynInt32`, `asynFloat64`, etc.) with `@asyn(PORT,ADDR,TIMEOUT)DRVINFO` links, handled by the universal asyn device support factory. No custom device support types are needed.

CP-linked records (MotorXPos, MotorYPos, BeamCurrent) use the 2-stage pattern: a Soft Channel link receiver forwards to the asyn record via `OUT PP`, separating CP link processing from asyn I/O.
