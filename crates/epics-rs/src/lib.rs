//! Pure Rust EPICS control system framework.
//!
//! This is the umbrella crate that re-exports the epics-rs sub-crates.
//! Use feature flags to select which modules you need:
//!
//! ```toml
//! [dependencies]
//! epics-rs = { version = "0.30", features = ["motor", "ad"] }
//! ```
//!
//! ## Features
//!
//! | Feature | Description | Default |
//! |---------|-------------|---------|
//! | `ca` | Channel Access client & server | yes |
//! | `pva` | pvAccess client & server | no |
//! | `bridge` | Record ↔ PVA bridge (QSRV equivalent); implies `pva` | no |
//! | `asyn` | Async port driver framework | no |
//! | `motor` | Motor record + SimMotor; implies `asyn` | no |
//! | `ad` | areaDetector (core + plugins); implies `asyn` | no |
//! | `ioc` | the areaDetector iocsh commands; implies `ad` | no |
//! | `calc` | Calc expression engine | always |
//! | `autosave` | PV save/restore | always |
//! | `busy` | Busy record | always |
//! | `std` | Standard records (epid, throttle, timestamp) | no |
//! | `scaler` | Scaler record (64-channel counter) | no |
//! | `optics` | Beamline optics (table, monochromator, filters) | no |
//! | `mca` | mca record (multichannel analyzer) | no |
//! | `full` | Everything | no |
//!
//! `calc`, `autosave` and `busy` are always available through `epics-base-rs`;
//! their feature flags exist so a dependant can name them and are empty.

/// Core IOC infrastructure — record system, database, iocsh, types.
pub use epics_base_rs as base;

/// Channel Access protocol — client and server.
#[cfg(feature = "ca")]
pub use epics_ca_rs as ca;

/// pvAccess protocol — client and server.
#[cfg(feature = "pva")]
pub use epics_pva_rs as pva;

/// Bridge: exposes EPICS records as pvAccess channels (QSRV equivalent).
#[cfg(feature = "bridge")]
pub use epics_bridge_rs as bridge;

/// Async port driver framework.
#[cfg(feature = "asyn")]
pub use asyn_rs as asyn;

/// Motor record + SimMotor.
#[cfg(feature = "motor")]
pub use motor_rs as motor;

/// areaDetector core — NDArray, driver base.
#[cfg(feature = "ad")]
pub use ad_core_rs as ad_core;

/// areaDetector plugins — Stats, ROI, FFT, file writers, etc.
#[cfg(feature = "ad")]
pub use ad_plugins_rs as ad_plugins;

/// Calc expression engine (re-exported from epics-base-rs).
pub mod calc {
    pub use epics_base_rs::calc::*;
}

/// PV automatic save/restore (re-exported from epics-base-rs).
pub mod autosave {
    pub use epics_base_rs::server::autosave::*;
}

/// Busy record (re-exported from epics-base-rs).
pub mod busy {
    pub use epics_base_rs::server::records::busy::*;
}

/// Standard records (epid, throttle, timestamp) and device support.
#[cfg(feature = "std")]
pub use std_rs as std_mod;

/// Scaler record — multi-channel counter with preset and auto-count support.
#[cfg(feature = "scaler")]
pub use scaler_rs as scaler;

/// Optics record types and device support (table, monochromator, filters, etc.).
#[cfg(feature = "optics")]
pub use optics_rs as optics;

/// mca record — multichannel analyzer, with the soft (simulated) driver.
#[cfg(feature = "mca")]
pub use mca_rs as mca;
