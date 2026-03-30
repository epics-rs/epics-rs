//! Pure Rust EPICS control system framework.
//!
//! This is the umbrella crate that re-exports all epics-rs sub-crates.
//! Use feature flags to select which modules you need:
//!
//! ```toml
//! [dependencies]
//! epics-rs = { version = "0.6", features = ["motor", "ad"] }
//! ```
//!
//! ## Features
//!
//! | Feature | Description | Default |
//! |---------|-------------|---------|
//! | `ca` | Channel Access client & server | yes |
//! | `pva` | pvAccess client (experimental) | no |
//! | `asyn` | Async port driver framework | no |
//! | `motor` | Motor record + SimMotor | no |
//! | `ad` | areaDetector (core + plugins) | no |
//! | `calc` | Calc expression engine | no |
//! | `autosave` | PV save/restore | no |
//! | `busy` | Busy record | no |
//! | `seq` | Sequencer runtime | no |
//! | `msi` | Macro substitution tool | no |
//! | `full` | Everything | no |

/// Core IOC infrastructure — record system, database, iocsh, types.
pub use epics_base_rs as base;

/// Channel Access protocol — client and server.
#[cfg(feature = "ca")]
pub use epics_ca_rs as ca;

/// pvAccess protocol — client (experimental).
#[cfg(feature = "pva")]
pub use epics_pva_rs as pva;

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

/// Calc expression engine.
#[cfg(feature = "calc")]
pub use epics_calc_rs as calc;

/// PV automatic save/restore.
#[cfg(feature = "autosave")]
pub use autosave_rs as autosave;

/// Busy record.
#[cfg(feature = "busy")]
pub use busy_rs as busy;

/// Sequencer runtime.
#[cfg(feature = "seq")]
pub use epics_seq_rs as seq;

