# optics-rs

Rust port of the EPICS synApps [optics](https://github.com/epics-modules/optics)
module for synchrotron beamline optical device control.

## Overview

optics-rs provides record types, state machine controllers, and device drivers
for monochromators, slits, filters, diffractometer tables, beam position
monitors, and other optical components.

| Component | Rust module | Original |
|---|---|---|
| 6-DOF optical table | `records::table` | tableRecord.c |
| 3x3 matrix math | `math::matrix3` | matrix3.c |
| 4-circle orientation | `math::orient` | orient.c |
| X-ray absorption data | `data::chantler` | chantler.c |
| Kohzu monochromator | `snl::kohzu_ctl` | kohzuCtl.st |
| Kohzu (soft motors) | `snl::kohzu_ctl_soft` | kohzuCtl_soft.st |
| HR analyzer crystal | `snl::hr_ctl` | hrCtl.st |
| Multi-layer mono | `snl::ml_mono_ctl` | ml_monoCtl.st |
| 4-circle diffractometer | `snl::orient` | orient_st.st |
| Auto filter selection | `snl::filter_drive` | filterDrive.st |
| XIA PF4 dual filter | `snl::pf4` | pf4.st |
| Ion chamber I0 | `snl::io` | Io.st |
| Coarse+fine flexure | `snl::flex_combined_motion` | flexCombinedMotion.st |
| HSC-1 slit controller | `drivers::hsc` | xiahsc.st / xia_slit.st |
| Quad X-ray BPM | `drivers::qxbpm` | sncqxbpm.st |

## Architecture

The original C module mixed three concerns into SNL (State Notation Language)
programs: device I/O, control logic, and physics calculations. This Rust port
separates them:

```
records/     Pure EPICS record types (table)
math/        Physics calculations (matrix3, orient)
data/        Reference data tables (chantler X-ray absorption)
snl/         Control logic as async state machines (epics-ca-rs)
drivers/     Device I/O as asyn port drivers (SimHsc, SimQxbpm)
db/          36 database templates from the original module
```

State machines (`snl/`) monitor PVs and drive motors. They contain no I/O
code and no EPICS record logic, only the control algorithm.

Port drivers (`drivers/`) own the hardware protocol. Each has a simulation
backend for testing without hardware.

## Usage in st.cmd

### State machines (control logic)

Start with `seqStart`, the Rust equivalent of the C EPICS `seq` command:

```bash
# Kohzu double-crystal monochromator
seqStart("kohzuCtl", "P=BL1:,M_THETA=dcm:theta,M_Y=dcm:y,M_Z=dcm:z")

# High-resolution analyzer
seqStart("hrCtl", "P=BL1:,N=1,M_PHI1=hr:phi1,M_PHI2=hr:phi2")

# Multi-layer monochromator
seqStart("ml_monoCtl", "P=BL1:,M_THETA=ml:theta,M_Y=ml:y,M_Z=ml:z")

# 4-circle diffractometer
seqStart("orient", "P=BL1:,PM=BL1:,mTTH=tth,mTH=th,mCHI=chi,mPHI=phi")

# Automatic filter selection (8 filters)
seqStart("filterDrive", "P=BL1:,R=filter:,N=8")

# XIA PF4 dual filter bank
seqStart("pf4", "P=BL1:,H=pf4:,B=A")

# Ion chamber intensity
seqStart("Io", "P=BL1:,MONO=BL1:mono:,VSC=BL1:scaler:")

# Coarse+fine flexure stage
seqStart("flexCombinedMotion", "P=BL1:,M=flex:,CAP=flex:cap:,FM=flex:fine,CM=flex:coarse")
```

### Serial device drivers

#### HSC-1 slit controller

```bash
# Simulation (no hardware)
simHscCreate("HSC1", 100)

# Real hardware
hscCreate("HSC1", "/dev/ttyUSB0", 9600, 100)

# Load slit database
dbLoadRecords("$(OPTICS)/db/xiahsc.db", "P=BL1:,HSC=HSC1:")
```

#### Quad X-ray BPM

```bash
# Simulation (beam at center)
simQxbpmCreate("QXBPM1", 0.0, 0.0, 100)

# Real hardware
qxbpmCreate("QXBPM1", "/dev/ttyUSB1", 9600, 100)

# Load BPM database
dbLoadRecords("$(OPTICS)/db/qxbpm.db", "P=BL1:,PORT=QXBPM1")
```

### Table record

The 6-DOF optical table record is registered as a record type:

```rust
// In your IOC binary
use optics_rs::records::table::TableRecord;

app = app.register_record_type("table", || Box::new(TableRecord::new()));
```

```bash
# st.cmd
dbLoadRecords("$(OPTICS)/db/table.db", "P=BL1:,Q=table1:,T=table1")
```

## Switching from simulation to real hardware

Simulation and real hardware use the same driver, database, and state
machines. Only the `Create` command changes:

```bash
# Development / testing
simMotorCreate("dcm_theta", -10, 90, 100)
simHscCreate("HSC1", 100)
simQxbpmCreate("QXBPM1", 0.0, 0.0, 100)

# Production (same DB, same seqStart, different drivers)
# motorCreate("dcm_theta", "/dev/ttyUSB0", ...)
# hscCreate("HSC1", "/dev/ttyUSB1", 9600, 100)
# qxbpmCreate("QXBPM1", "/dev/ttyUSB2", 9600, 100)
```

## Available seqStart programs

| Program | Required macros | Optional macros |
|---|---|---|
| `kohzuCtl` | P, M_THETA, M_Y, M_Z | GEOM |
| `kohzuCtl_soft` | P, M_THETA, M_Y, M_Z | MONO, GEOM |
| `hrCtl` | P, M_PHI1, M_PHI2 | N |
| `ml_monoCtl` | P, M_THETA | M_THETA2, M_Y, M_Z, Y_OFFSET, GEOM |
| `orient` | P, PM, mTTH, mTH, mCHI, mPHI | |
| `filterDrive` | P, R | N |
| `pf4` | P, B | H |
| `Io` | P | MONO, VSC |
| `flexCombinedMotion` | P, M, FM, CM | CAP |

## Database templates

36 templates are bundled in the `db/` directory. Key templates:

| Template | Description |
|---|---|
| `kohzuSeq.db` | Kohzu DCM (60 records: energy, wavelength, Bragg angle) |
| `table.db` | 6-DOF optical table |
| `2slit.db` | Two-blade slit (gap/center) |
| `orient.db` | Crystal orientation matrix |
| `hrSeq.db` | High-resolution monochromator |
| `ml_monoSeq.db` | Multi-layer monochromator |
| `filterDrive.db` | Filter selection |
| `pf4bank.db` / `pf4common.db` | XIA PF4 filter banks |
| `fb_epid.db` | Feedback PID loop |
| `xiahsc.db` | XIA HSC-1 slit |
| `xia_slit.db` | XIA slit with scan support |
| `qxbpm.db` | Quad BPM |
| `bragg.db` | Simple Bragg angle calculation |
| `Io.db` | Ion chamber |
| `SGM.db` | Spherical grating monochromator |

## Testing

362 tests covering:

- **Golden tests** (46): Rust output compared against values from the original
  C `tableRecord.c`, compiled and executed independently. Tolerance: 1e-10.
- **Matrix/orient** (10): Round-trip verification against published
  crystallographic data (Si, Be, VO2) from the original optics test suite.
- **Chantler** (8): X-ray absorption coefficients for 22 elements.
- **State machines** (127): Physics calculations for each controller.
- **Serial protocol** (111): Command formatting, response parsing, coordinate
  math for HSC-1 and QXBPM.
- **Port drivers** (23): SimHsc and SimQxbpm parameter updates and poll loops.
- **Table record** (32): Field access, geometry modes, process logic.
- **seq_runner** (3): Macro parsing, program dispatch.

```sh
cargo test -p optics-rs
```

## Quick Start: Kohzu DCM Simulation

Build and run the mini-beamline IOC with a simulated Kohzu double-crystal monochromator:

```bash
cargo build --release -p mini-beamline --features ioc
./target/release/mini_ioc examples/mini-beamline/ioc/st.cmd
```

In another terminal, set the DCM to Auto mode and change energy:

```bash
# Switch to Auto mode (Manual mode calculates but doesn't move motors)
❯ caput-rs mini:KohzuModeBO 1
Old : mini:KohzuModeBO 0
New : mini:KohzuModeBO 1

# Set energy to 8.0 keV — the theta motor moves to the Bragg angle
❯ caput-rs mini:BraggEAO 8.0
Old : mini:BraggEAO 113.28186997002285
New : mini:BraggEAO 8.0
```

Monitor the DCM motor and readback PVs:

```bash
# Watch the theta motor position (SimMotor moves in real time)
❯ camonitor-rs mini:dcm:theta.RBV
mini:dcm:theta.RBV 2026-04-01 01:10:56.954208 0
mini:dcm:theta.RBV 2026-04-01 01:10:58.057360 14.308

# Read back the computed values
❯ caget-rs mini:BraggThetaRdbkAO
mini:BraggThetaRdbkAO 14.307754265176753

❯ caget-rs mini:BraggLambdaRdbkAO
mini:BraggLambdaRdbkAO 1.54980305

❯ caget-rs mini:BraggERdbkAO
mini:BraggERdbkAO 8.0
```

Change energy and watch the motor track:

```bash
❯ caput-rs mini:BraggEAO 12.0
❯ camonitor-rs mini:dcm:theta.RBV
mini:dcm:theta.RBV 2026-04-01 01:11:02.161521 14.308
mini:dcm:theta.RBV 2026-04-01 01:11:03.264680 9.483
```

### Key PVs

| PV | Type | Description |
|----|------|-------------|
| `mini:BraggEAO` | ao | Energy setpoint (keV) |
| `mini:BraggERdbkAO` | ao | Energy readback (keV) |
| `mini:BraggLambdaAO` | ao | Wavelength setpoint (A) |
| `mini:BraggLambdaRdbkAO` | ao | Wavelength readback (A) |
| `mini:BraggThetaAO` | ao | Theta setpoint (deg) |
| `mini:BraggThetaRdbkAO` | ao | Theta readback (deg) |
| `mini:KohzuModeBO` | bo | Manual(0) / Auto(1) |
| `mini:KohzuMoving` | busy | Moving indicator |
| `mini:KohzuSeqMsg1SI` | stringin | Status message |
| `mini:dcm:theta` | motor | Theta motor record |
| `mini:dcm:theta.RBV` | motor | Theta motor readback |
| `mini:dcm:y` | motor | Y motor record |
| `mini:dcm:z` | motor | Z motor record |

### Crystal parameters

Default: Si (111), lattice constant a = 5.43102 A

```bash
# Change to Si (220)
caput-rs mini:BraggHAO 2
caput-rs mini:BraggKAO 2
caput-rs mini:BraggLAO 0

# Check the 2d spacing
caget-rs mini:Bragg2dSpacingAO
```
