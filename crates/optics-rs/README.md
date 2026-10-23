# optics-rs

Rust port of the EPICS synApps [optics](https://github.com/epics-modules/optics)
module for synchrotron beamline optical device control.

## Overview

optics-rs provides the table record, the module's twelve sequence programs
and its math for monochromators, slits, filters, diffractometer tables, beam
position monitors, and other optical components.

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
| HSC-1 slit controller | `snl::xiahsc` | xiahsc.st |
| HSC-1 slit, scannable (busy records) | `snl::xia_slit` | xia_slit.st |
| Quad X-ray BPM | `snl::qxbpm` | sncqxbpm.st |
| Simulated HSC-1 and quad BPM | `drivers::hsc`, `drivers::qxbpm` | none (rs only) |

Each `snl` module is the upstream `.st`, with C defects fixed in place under
a comment, compiled by epics-snc-rs in `build.rs` and run on epics-seq-rs; the `.rs` beside it supplies what the
program's escaped C supplied. As in C, each state set runs on its own OS
thread.

## Architecture

```
records/       The table record
math/          matrix3, orient
data/          chantler X-ray absorption tables
snl/           The twelve upstream .st programs and their companions
drivers/       Simulated asyn ports for the HSC-1 and the quad BPM
seq_runner.rs  seq_start: start a compiled program by name
db/            32 database templates and 4 .vdb from the original module
ui/            PyDM screens
```

`xiahsc`, `xia_slit` and `sncqxbpm` talk to the device themselves, through
the asyn record their `S` macro names, as in C. `drivers/` has no C
counterpart: asyn ports over an in-memory HSC-1 (blade, gap and center
positions) and quad BPM (diode currents and beam position), for running
without hardware.

## Usage in st.cmd

### State machines (control logic)

`seqStart` is the startup command an IOC registers around
`optics_rs::seq_runner::seq_start` (`examples/mini-beamline` registers it),
its stand-in for C's `seq`. A macro left out takes the default on the
program's `.st` `program` line.

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
seqStart("filterDrive", "NAME=filters,P=BL1:,R=filter:,NUM_FILTERS=8")

# XIA PF4 dual filter bank
seqStart("pf4", "name=pf4_1,P=BL1:,H=pf4:,B=A,M=BL1:userTranpf4.I,B1=BL1:userTranpf4.A,B2=BL1:userTranpf4.B,B3=BL1:userTranpf4.C,B4=BL1:userTranpf4.D")

# Ion chamber intensity
seqStart("Io", "P=BL1:Io:,MONO=BL1:BraggERdbkAO,VSC=BL1:scaler1")

# Coarse+fine flexure stage
seqStart("flexCombinedMotion", "P=BL1:,M=flex1,C=cap1,FM=flex:fine,CM=flex:coarse")

# HSC-1 slit on the asyn record BL1:asyn_hsc, with $(OPTICS)/db/xiahsc.db loaded
seqStart("xiahsc", "name=hsc1,P=BL1:,HSC=hsc1:,S=BL1:asyn_hsc")
```

A program can also be started directly, as the tests do:

```rust
epics_seq_rs::seq(&optics_rs::snl::kohzu_ctl::kohzuCtl, "P=BL1:,M_THETA=dcm:theta", 0, &db);
```

### Simulated serial devices

`HscHolder::sim_hsc_create_command()` and
`QxbpmHolder::sim_qxbpm_create_command()` give an IOC the `simHscCreate` and
`simQxbpmCreate` commands. Each makes an asyn port; `examples/mini-beamline`
loads its `hsc.template` and `qxbpm.template` on them:

```bash
simHscCreate("HSC1", 100)                # port, pollMs
simQxbpmCreate("QXBPM1", 0.0, 0.0, 100)  # port, xPos, yPos, pollMs
dbLoadRecords("$(MINI_BEAMLINE)/db/hsc.template", "P=$(PREFIX),HSC=hsc1:,PORT=HSC1")
dbLoadRecords("$(MINI_BEAMLINE)/db/qxbpm.template", "P=$(PREFIX)bpm1:,PORT=QXBPM1")
```

### Table record

The 6-DOF optical table record is registered as a record type;
`table_record_factory()` also loads the record's `.dbd`:

```rust
// In your IOC binary
let (name, factory) = optics_rs::table_record_factory();
app = app.register_record_type(name, factory);
```

```bash
# st.cmd
dbLoadRecords("$(OPTICS)/db/table.db", "P=BL1:,Q=Table1,T=table1,M0X=m1,M0Y=m2,M1Y=m3,M2X=m4,M2Y=m5,M2Z=m6,GEOM=SRI")
```

## Available programs

Each program's macros, as its `.st` `program` line lists them; that line
also gives each one's default.

| Program | `.st` | Macros |
|---|---|---|
| `kohzuCtl` | kohzuCtl.st | P, M_THETA, M_Y, M_Z, GEOM |
| `kohzuCtl_soft` | kohzuCtl_soft.st | P, MONO, M_THETA, M_Y, M_Z, GEOM |
| `hrCtl` | hrCtl.st | P, N, M_PHI1, M_PHI2 |
| `ml_monoCtl` | ml_monoCtl.st | P, M_THETA, M_THETA2, M_Y, M_Z, Y_OFF, GEOM |
| `orient` | orient_st.st | P, PM, mTTH, mTH, mCHI, mPHI |
| `filterDrive` | filterDrive.st | NAME, P, R, NUM_FILTERS |
| `pf4` | pf4.st | name, P, H, B, BP, B1..B4, M |
| `Io` | Io.st | P, MONO, VSC |
| `flexCombinedMotion` | flexCombinedMotion.st | name, P, M, C, FM, CM |
| `xiahsc` | xiahsc.st | name, S, P, HSC |
| `xia_slit` | xia_slit.st | name, P, HSC, S |
| `sncqxbpm` | sncqxbpm.st | name, P, S |

## Database templates

32 `.db` templates and 4 `.vdb` files are bundled in `db/`. Key templates:

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

What the suite covers:

- **Golden tests**: Rust output compared against values from the original
  C `tableRecord.c`, compiled and executed independently. Tolerance: 1e-10.
- **Matrix/orient**: Round-trip verification against published
  crystallographic data (Si, Be, VO2) from the original optics test suite.
- **Chantler**: X-ray absorption coefficients for 22 elements.
- **State machines**: each compiled `.st` run on its shipped `.db`.
- **Serial devices**: `xiahsc`, `xia_slit` and `sncqxbpm` against mock
  HSC-1 and quad BPM devices on TCP behind an asyn record; the C programs
  ran against the same mocks for the expected values.
- **Port drivers**: SimHsc and SimQxbpm parameter updates and poll loops.
- **Table record**: Field access, geometry modes, process logic.
- **seq_runner**: program lookup, macro defaults, start before PINI.

```sh
cargo nextest run -p optics-rs
```

## Quick Start: Kohzu DCM Simulation

Build and run the mini-beamline IOC with a simulated Kohzu double-crystal monochromator:

```bash
cargo build --release -p mini-beamline --features ioc
./target/release/mini-ioc examples/mini-beamline/ioc/st.cmd
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

## License

[EPICS Open License](../../LICENSE)
