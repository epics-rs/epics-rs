# Scope Simulator IOC

A digital oscilloscope simulator IOC for epics-rs, ported from the EPICS
[testAsynPortDriver](https://github.com/epics-modules/asyn/blob/master/testAsynPortDriverApp/src/testAsynPortDriver.cpp)
example.

Generates a 1 kHz sine waveform (1000 points) with configurable noise,
vertical gain, time/volts per division, trigger delay, and voltage offset.
All readback records update via I/O Intr scanning.

## Devices

### Scope Simulator

An asyn PortDriver that simulates a digital oscilloscope:

- **Signal**: 1 kHz sine wave, amplitude 1.0 V
- **Waveform**: 1000-point buffer covering `NUM_DIVISIONS * TimePerDiv` seconds
- **Background task**: recomputes waveform at the configured `UpdateTime` interval
- **Noise**: additive uniform noise with configurable amplitude
- **Vertical gain**: x1 / x2 / x5 / x10 (rescales volts-per-division choices)
- **Statistics**: min, max, mean computed per waveform update

## PV Reference

Default prefix: `SCOPE:scopeSim:`

### Control (output records)

| PV | Type | Description |
|----|------|-------------|
| `Run` | bo | Start/stop waveform generation |
| `UpdateTime` | ao | Waveform update interval (s, min 0.02) |
| `VoltOffset` | ao | Voltage offset (V) |
| `TriggerDelay` | ao | Trigger delay (s) |
| `NoiseAmplitude` | ao | Noise amplitude (V) |
| `TimePerDivSelect` | mbbo | Time per division (0.001 - 1.0 ms) |
| `VertGainSelect` | mbbo | Vertical gain (x1 / x2 / x5 / x10) |
| `VoltsPerDivSelect` | mbbo | Volts per division (auto-scaled by gain) |

### Readback (input records, I/O Intr)

| PV | Type | Description |
|----|------|-------------|
| `Run_RBV` | bi | Running status |
| `MaxPoints_RBV` | longin | Number of waveform points (1000) |
| `UpdateTime_RBV` | ai | Update interval readback |
| `VoltOffset_RBV` | ai | Voltage offset readback |
| `TriggerDelay_RBV` | ai | Trigger delay readback |
| `NoiseAmplitude_RBV` | ai | Noise amplitude readback |
| `VertGain_RBV` | ai | Vertical gain value readback |
| `TimePerDiv_RBV` | ai | Time per division readback (s) |
| `VoltsPerDivSelect_RBV` | mbbi | Volts per division selection readback |
| `VoltsPerDiv_RBV` | ai | Volts per division value readback |
| `MinValue_RBV` | ai | Waveform minimum |
| `MaxValue_RBV` | ai | Waveform maximum |
| `MeanValue_RBV` | ai | Waveform mean |

### Waveform (I/O Intr)

| PV | Type | Description |
|----|------|-------------|
| `Waveform_RBV` | waveform | Waveform data (DOUBLE, 1000 elements) |
| `TimeBase_RBV` | waveform | Time base (DOUBLE, 1000 elements) |

## Build and Run

### IOC (with Channel Access)

```bash
# Release build
cargo build --release -p scope-ioc --features ioc
# Run
./target/release/scope-ioc examples/scope-ioc/ioc/st.cmd
```

### Standalone Demo (no EPICS)

```bash
cargo run -p scope-ioc --example scope_sim
```

### Verify

```bash
# Start acquisition
caput SCOPE:scopeSim:Run 1

# Monitor waveform statistics
camonitor SCOPE:scopeSim:MinValue_RBV SCOPE:scopeSim:MaxValue_RBV SCOPE:scopeSim:MeanValue_RBV

# Add noise
caput SCOPE:scopeSim:NoiseAmplitude 0.2

# Change vertical gain to x10
caput SCOPE:scopeSim:VertGainSelect 3

# Read waveform
caget -# SCOPE:scopeSim:Waveform_RBV

# Stop
caput SCOPE:scopeSim:Run 0

# Driver status (from iocsh)
scopeSimulatorReport
```

## OPI Screens

MEDM and PyDM operator interface screens are included in `opi/`:

- `opi/medm/testAsynPortDriverTop.adl` — top-level MEDM display
- `opi/pydm/testAsynPortDriverTop.ui` — top-level PyDM display
