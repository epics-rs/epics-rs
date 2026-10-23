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

### Area Detector (SimDetector)

The [`sim-detector`](../sim-detector) driver, configured by
`simDetectorConfig("SIM", 640, 480, 1, 0, 50000000)`, with ADSimDetector's
`simDetector.template`, an `image1` NDStdArrays plugin and ADCore's
`commonPlugins.cmd`.

| PV | Description |
|----|-------------|
| `XF:31IDA-BI{Cam:Tbl}:cam1:Acquire` | Start/stop acquisition |
| `XF:31IDA-BI{Cam:Tbl}:cam1:AcquireTime` | Exposure time (s) |
| `XF:31IDA-BI{Cam:Tbl}:cam1:ImageMode` | Single / Multiple / Continuous |
| `XF:31IDA-BI{Cam:Tbl}:image1:ArrayData` | Image data waveform (`FTVL=SHORT`, 307200 elements) |

The detector and plugin records load twice, under the `XF:31IDA-BI{Cam:Tbl}:`
and `ADSIM:` prefixes, for ophyd test compatibility. The second set of
Configure lines fails as in C, because the ports already exist; only the
records load under the new prefix.

The library also carries a MovingDot driver (`ophyd_test_ioc::sim_detector::create_moving_dot`,
`db/sim_cam.template`); the binary does not load it.

## Build and Run

```bash
cargo build --release -p ophyd-test-ioc --features ioc
./target/release/ophyd-test-ioc examples/ophyd-test-ioc/ioc/st.cmd
```

## Architecture

Motors are `motor.template` records (`DTYP asynMotor`) on `simMotorCreate`
ports. Sensors are Soft Channel `ai` records from `db/sensor.template`.
Detector and plugin records use the standard asyn DTYPs (`asynInt32`,
`asynFloat64`, etc.) with `@asyn(PORT,ADDR,TIMEOUT)DRVINFO` links, handled by
the universal asyn device support factory.
