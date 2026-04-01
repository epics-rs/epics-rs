# Mini Beamline IOC

A beamline simulation IOC for epics-rs, inspired by
[caproto](https://github.com/caproto/caproto)'s
[`ioc_examples/mini_beamline.py`](https://github.com/caproto/caproto/blob/master/caproto/ioc_examples/mini_beamline.py).

Provides a simulated beam current, three 1D point detectors, one 2D area detector (MovingDot), a Kohzu double-crystal monochromator (DCM), an HSC-1 slit controller, a quad BPM, and eight simulation motors.

## Devices

### Beam Current

Simulates a sinusoidal beam current:

```
I(t) = OFFSET + AMPLITUDE * sin(2*pi*t / PERIOD)
```

Updated periodically by a background thread and delivered to clients via I/O Intr scanning.

### Point Detectors

Each point detector takes motor position and beam current as inputs and computes a scalar detector value. Three modes are available:

| Name | Mode | Formula | Default sigma | Default center |
|------|------|---------|---------------|----------------|
| PinHole | Gaussian peak | `N * I * exp * e^(-(mtr-center)^2 / 2*sigma^2)` | 5.0 | 0.0 |
| Edge | Error function | `N * I * exp * erfc((center-mtr) / sigma) / 2` | 2.5 | 5.0 |
| Slit | Double error function | `N * I * exp * (erfc((mtr-center)/sigma) - erfc((mtr+center)/sigma)) / 2` | 2.5 | 7.5 |

The detector value is automatically recomputed whenever the motor RBV changes via CP links.

### MovingDot (2D Area Detector)

An area detector that produces images with a 2D Gaussian spot that moves according to two motor axes (X, Y).
Follows the ADDriver pattern and supports Single/Multiple/Continuous image modes.

- Gaussian spot: `sigma_x=50, sigma_y=25` (pixels, configurable)
- Background noise: `Poisson(lambda=1000)`
- When the shutter is closed, only background noise is produced (dark frame)

### Kohzu DCM (Double Crystal Monochromator)

Simulates a Kohzu-style double-crystal monochromator using the `kohzuCtl` state machine from `optics-rs`. Three `SimMotor` axes (Theta, Y, Z) are driven by the controller based on energy setpoint.

| PV | Type | Description |
|----|------|-------------|
| `mini:BraggEAO` | ao | Energy setpoint (keV) |
| `mini:BraggERdbkAO` | ao | Energy readback (keV) |
| `mini:BraggLambdaRdbkAO` | ao | Wavelength readback (A) |
| `mini:BraggThetaRdbkAO` | ao | Theta readback (deg) |
| `mini:KohzuMoving` | busy | Moving indicator |
| `mini:KohzuModeBO` | bo | Manual(0) / Auto(1) |

Set the energy and the controller calculates the Bragg angle, then drives the theta motor:

```bash
caput mini:BraggEAO 8.0
caget mini:BraggThetaRdbkAO
camonitor mini:dcm:theta.RBV
```

### HSC-1 Slit Controller

Simulated XIA HSC-1 four-blade slit using `SimHsc` from `optics-rs`. Supports gap/center and individual blade control.

| PV | Description |
|----|-------------|
| HSC parameters | Exposed via asyn port driver (H_GAP, H_CENTER, TOP, BOTTOM, LEFT, RIGHT) |

### Quad BPM

Simulated Oxford quad X-ray beam position monitor using `SimQxbpm`. Reports X/Y beam position from four simulated diode currents.

| PV | Description |
|----|-------------|
| QXBPM parameters | Exposed via asyn port driver (X_POS, Y_POS, CURRENT_A-D) |

### Motors

Eight full MotorRecords using `SimMotor` from `motor-rs`:

| PV | Description |
|----|-------------|
| `mini:ph:mtr` | PinHole detector motor |
| `mini:edge:mtr` | Edge detector motor |
| `mini:slit:mtr` | Slit detector motor |
| `mini:dot:mtrx` | MovingDot X-axis motor |
| `mini:dot:mtry` | MovingDot Y-axis motor |
| `mini:dcm:theta` | DCM Theta motor (-10 to 90 deg) |
| `mini:dcm:y` | DCM Y motor |
| `mini:dcm:z` | DCM Z motor |

## PV Reference

### Beam Current

| PV | Type | Description |
|----|------|-------------|
| `mini:current` | ai | Beam current (mA, I/O Intr) |

### Point Detectors

For each `R` = `ph:`, `edge:`, `slit:`:

| PV | Type | Description |
|----|------|-------------|
| `mini:{R}MotorPos` | ao | Motor position (CP link from motor RBV) |
| `mini:{R}BeamCurrent` | ao | Beam current (CP link from mini:current) |
| `mini:{R}ExposureTime` | ao | Exposure time (s, user-settable) |
| `mini:{R}ExposureTime_RBV` | ai | Exposure time readback |
| `mini:{R}DetValue_RBV` | ai | Detector value (I/O Intr) |
| `mini:{R}DetSigma` | ao | Detector sigma (user-settable) |
| `mini:{R}DetSigma_RBV` | ai | Sigma readback |
| `mini:{R}DetCenter` | ao | Detector center (user-settable) |
| `mini:{R}DetCenter_RBV` | ai | Center readback |

### MovingDot Area Detector

| PV | Type | Description |
|----|------|-------------|
| `mini:dot:cam:Acquire` | bo | Start/stop acquisition |
| `mini:dot:cam:Acquire_RBV` | bi | Acquisition status readback |
| `mini:dot:cam:ImageMode` | mbbo | Single / Multiple / Continuous |
| `mini:dot:cam:ImageMode_RBV` | mbbi | Image mode readback |
| `mini:dot:cam:NumImages` | longout | Number of images (Multiple mode) |
| `mini:dot:cam:NumImages_RBV` | longin | Number of images readback |
| `mini:dot:cam:NumImagesCounter_RBV` | longin | Images acquired so far |
| `mini:dot:cam:AcquireTime` | ao | Exposure time (s) |
| `mini:dot:cam:AcquireTime_RBV` | ai | Exposure time readback |
| `mini:dot:cam:AcquirePeriod` | ao | Acquisition period (s) |
| `mini:dot:cam:AcquirePeriod_RBV` | ai | Acquisition period readback |
| `mini:dot:cam:DetectorState_RBV` | mbbi | Detector state (Idle/Acquire/...) |
| `mini:dot:cam:AcquireBusy_RBV` | bi | Whether acquisition is in progress |
| `mini:dot:cam:ArrayCounter` | longout | Frame counter (resettable) |
| `mini:dot:cam:ArrayCounter_RBV` | longin | Frame counter readback |
| `mini:dot:cam:ArrayCallbacks` | bo | Enable/disable NDArray callbacks |
| `mini:dot:cam:ArrayCallbacks_RBV` | bi | Callback status readback |
| `mini:dot:cam:MaxSizeX_RBV` | longin | Maximum image width |
| `mini:dot:cam:MaxSizeY_RBV` | longin | Maximum image height |
| `mini:dot:cam:SizeX` | longout | Image width |
| `mini:dot:cam:SizeX_RBV` | longin | Width readback |
| `mini:dot:cam:SizeY` | longout | Image height |
| `mini:dot:cam:SizeY_RBV` | longin | Height readback |
| `mini:dot:cam:MotorXPos` | ao | X motor position (CP from dot:mtrx RBV) |
| `mini:dot:cam:MotorXPos_RBV` | ai | X motor position readback |
| `mini:dot:cam:MotorYPos` | ao | Y motor position (CP from dot:mtry RBV) |
| `mini:dot:cam:MotorYPos_RBV` | ai | Y motor position readback |
| `mini:dot:cam:BeamCurrent` | ao | Beam current (CP from mini:current) |
| `mini:dot:cam:BeamCurrent_RBV` | ai | Beam current readback |
| `mini:dot:cam:ShutterOpen` | bo | Shutter open/closed |
| `mini:dot:cam:ShutterOpen_RBV` | bi | Shutter status readback |
| `mini:dot:cam:Manufacturer_RBV` | stringin | Manufacturer ("Mini Beamline") |
| `mini:dot:cam:Model_RBV` | stringin | Model name ("Moving Dot") |

## Configuration

All simulation parameters can be changed in `ioc/st.cmd` via `epicsEnvSet` before IOC startup.
Values must be set before the `miniBeamlineConfig()` call.

### Beam Current

| Variable | Default | Description |
|----------|---------|-------------|
| `BEAM_OFFSET` | 500.0 | DC offset (mA) |
| `BEAM_AMPLITUDE` | 25.0 | Oscillation amplitude (mA) |
| `BEAM_PERIOD` | 4.0 | Oscillation period (s) |
| `BEAM_UPDATE_MS` | 100 | Update interval (ms) |

### Motors

Motors are configured in `st.cmd` using `simMotorCreate` and `dbLoadRecords`:

```
# simMotorCreate(port, lowLimit, highLimit, [pollMs])
simMotorCreate("ph_mtr", -100, 100, 100)

# dbLoadRecords(template, macros)
#   VELO, ACCL, HLM, LLM, MRES, PREC have defaults in motor.template
dbLoadRecords("$(MOTOR)/motor.template", "P=mini:,M=ph:mtr,PORT=ph_mtr")
```

### MovingDot

| Variable | Default | Description |
|----------|---------|-------------|
| `DOT_SIZE_X` | 640 | Image width (px) |
| `DOT_SIZE_Y` | 480 | Image height (px) |
| `DOT_MAX_MEMORY` | 50000000 | NDArray pool max memory (bytes) |
| `DOT_SIGMA_X` | 50.0 | Gaussian spot X sigma (px) |
| `DOT_SIGMA_Y` | 25.0 | Gaussian spot Y sigma (px) |
| `DOT_BACKGROUND` | 1000.0 | Background noise (Poisson lambda) |
| `DOT_N_PER_I_PER_S` | 200.0 | Photons per mA per second |

### Optics Devices

Optics devices are configured in `st.cmd`:

```bash
# Kohzu DCM: 3 SimMotors + kohzuSeq.db + kohzuCtl state machine
simMotorCreate("dcm_theta", -10, 90, 100)
simMotorCreate("dcm_y", -50, 50, 100)
simMotorCreate("dcm_z", -50, 50, 100)
dbLoadRecords("$(MOTOR)/motor.template", "P=mini:,M=dcm:theta,PORT=dcm_theta")
dbLoadRecords("$(MOTOR)/motor.template", "P=mini:,M=dcm:y,PORT=dcm_y")
dbLoadRecords("$(MOTOR)/motor.template", "P=mini:,M=dcm:z,PORT=dcm_z")
dbLoadRecords("$(OPTICS)/db/kohzuSeq.db", "P=mini:,M_THETA=dcm:theta,M_Y=dcm:y,M_Z=dcm:z")
seqStart("kohzuCtl", "P=mini:,M_THETA=dcm:theta,M_Y=dcm:y,M_Z=dcm:z")

# HSC-1 slit: SimHsc port driver
simHscCreate("HSC1", 100)

# Quad BPM: SimQxbpm port driver (beam at center)
simQxbpmCreate("QXBPM1", 0.0, 0.0, 100)
```

To switch from simulation to real hardware, replace the `sim*Create` commands:

```bash
# Real hardware (same DB templates, same seqStart)
# motorCreate("dcm_theta", "/dev/ttyUSB0", ...)
# hscCreate("HSC1", "/dev/ttyUSB1", 9600, 100)
# qxbpmCreate("QXBPM1", "/dev/ttyUSB2", 9600, 100)
```

### Example

```
# ioc/st.cmd — fast beam, small image for testing
epicsEnvSet("BEAM_PERIOD",    "1.0")
epicsEnvSet("BEAM_AMPLITUDE", "50.0")
epicsEnvSet("DOT_SIZE_X",     "128")
epicsEnvSet("DOT_SIZE_Y",     "96")
epicsEnvSet("MOTOR_VELO",     "10.0")
```

## Architecture

### IOC startup phases

The IOC follows the standard C EPICS startup sequence:

1. **Phase 1 (st.cmd):** A blocking thread executes the startup script. Commands like `miniBeamlineConfig()`, `simMotorCreate()`, and `dbLoadRecords()` create drivers and load record definitions.
2. **Phase 2 (iocInit):** The framework wires device support to records by matching each record's DTYP field to a registered factory, then calls `DeviceSupport::init()` and sets up I/O Intr scanning.
3. **Phase 3 (shell):** An interactive iocsh REPL for runtime inspection (`dbl`, `dbgf`, `dbpf`, etc.).

### Record, DeviceSupport, and Driver

The IOC uses a three-layer architecture. Each layer has a single responsibility and communicates only with its immediate neighbors:

```
Record  <-->  DeviceSupport  <-->  Driver
(data)        (translation)        (hardware)
```

**Record** — holds PV fields (VAL, RBV, DTYP, SCAN, ...) and runs the process cycle. Created by `dbLoadRecords()` from `.template` files. This is what Channel Access clients see and interact with.

**DeviceSupport** — translates between records and drivers. On `read()`, it fetches a value from the driver and writes it into the record. On `write()`, it reads the record's value and sends a command to the driver. Each DeviceSupport instance is bound to exactly one record during iocInit.

**Driver** — controls the actual hardware (or simulator). It knows nothing about EPICS records. It exposes a domain-specific interface (`AsynMotor::move_absolute()`, `PortDriver::write_int32()`, etc.).

| Layer | This IOC's instances | Trait |
|-------|---------------------|-------|
| Record | MotorRecord, AiRecord, AoRecord, ... | `Record` |
| DeviceSupport | `MotorDeviceSupport`, `BeamCurrentDeviceSupport`, `PointDetectorDeviceSupport`, `MovingDotDeviceSupport` | `DeviceSupport` |
| Driver | `SimMotor`, beam current thread, `PointDetectorRuntime`, `MovingDotRuntime` | `AsynMotor`, `PortDriver`, ... |

Swapping the driver changes the hardware; swapping the device support changes how records map to the driver. The record layer stays the same either way.

### Phase bridge (BeamlineHolder)

Drivers are created during Phase 1 (st.cmd thread), but device support factories run during Phase 2 (tokio thread). `BeamlineHolder` bridges this gap — the config command stores driver handles into it, and the factories read them back out. This is the Rust equivalent of the global variables that C EPICS IOCs use to pass driver handles from `xxxConfigure()` to device support `init()`.

### Template-based motors

Motors use the `simMotorCreate` + `dbLoadRecords("motor.template", ...)` pattern instead of hardcoded Rust. The `simMotorCreate` command creates a `SimMotor` driver and spawns its poll loop. The template creates a `MotorRecord` with a matching DTYP. During iocInit, `DeviceSupport::init()` injects the `SharedDeviceState` into the record via `as_any_mut()` downcast, completing the wiring.

## Build and Run

```bash
# Release build (optimized)
cargo build --release -p mini-beamline --features ioc

# Run
./target/release/mini_ioc examples/mini-beamline/ioc/st.cmd
```

The CA server port can be changed with the `EPICS_CA_SERVER_PORT` environment variable (default: 5064).

### Verify

```bash
# Set DCM energy and watch the theta motor move
caput mini:BraggEAO 8.0
camonitor mini:dcm:theta.RBV
caget mini:BraggThetaRdbkAO

# Monitor beam current
camonitor mini:current

# Move motor and check detector value
caput mini:ph:mtr 0
camonitor mini:ph:DetValue_RBV

# Move motor away from center — detector value decreases
caput mini:ph:mtr 20

# Acquire a MovingDot image
caput mini:dot:cam:ArrayCallbacks 1
caput mini:dot:cam:ImageMode 0        # Single
caput mini:dot:cam:AcquireTime 0.1
caput mini:dot:cam:Acquire 1
caget mini:dot:cam:ArrayCounter_RBV
```
