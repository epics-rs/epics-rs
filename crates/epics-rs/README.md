# epics-rs

Umbrella crate that re-exports all epics-rs sub-crates. Use feature flags to select which modules you need.

**Repository:** <https://github.com/epics-rs/epics-rs>

## Features

| Feature | Description | Default |
|---------|-------------|---------|
| `ca` | Channel Access client & server | **yes** |
| `pva` | pvAccess client & server | no |
| `bridge` | Record <-> PVA bridge (QSRV equivalent); implies `pva` | no |
| `asyn` | Async port driver framework | no |
| `motor` | Motor record + SimMotor; implies `asyn` | no |
| `ad` | areaDetector (core + 26 plugins); implies `asyn` | no |
| `ioc` | the areaDetector iocsh commands; implies `ad` | no |
| `calc` | Calc expression engine | always |
| `autosave` | PV save/restore | always |
| `busy` | Busy record | always |
| `std` | Standard records (epid, throttle, timestamp) | no |
| `scaler` | Scaler record (64-channel counter) | no |
| `optics` | Beamline optics (table, monochromator, filters) | no |
| `mca` | mca record (multichannel analyzer) | no |
| `full` | Everything | no |

`calc`, `autosave` and `busy` are rows for discoverability only — they reach
you through `epics-base-rs`, so the features select nothing.

## Usage

```toml
[dependencies]
epics-rs = { version = "0.30", features = ["motor", "ad"] }
```

```rust
use epics_rs::base;        // IOC runtime, records, iocsh
use epics_rs::ca;          // Channel Access (feature = "ca")
use epics_rs::pva;         // pvAccess client and server (feature = "pva")
use epics_rs::bridge;      // Record <-> PVA bridge (feature = "bridge")
use epics_rs::asyn;        // port driver framework (feature = "asyn")
use epics_rs::motor;       // motor record (feature = "motor")
use epics_rs::ad_core;     // areaDetector core (feature = "ad")
use epics_rs::ad_plugins;  // areaDetector plugins (feature = "ad")
use epics_rs::std_mod;     // std records — not `std` (feature = "std")
use epics_rs::scaler;      // scaler record (feature = "scaler")
use epics_rs::optics;      // beamline optics (feature = "optics")
use epics_rs::mca;         // mca record (feature = "mca")
```

## License

[EPICS Open License](../../LICENSE)
