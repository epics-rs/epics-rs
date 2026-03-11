//! IOC support for areaDetector plugins.
//!
//! Provides reusable infrastructure for building IOC binaries with
//! areaDetector plugins:
//!
//! - [`DriverContext`] trait: abstraction for driver runtime (pool, downstream wiring)
//! - [`PluginManager`]: manages plugin lifecycle, port registration, device support dispatch
//! - [`PluginDeviceSupport`]: bridges EPICS records to plugin asyn ports
//! - Helper functions for C-compatible plugin configure commands

mod driver_context;
mod helpers;
mod plugin_device_support;
mod plugin_manager;

pub use driver_context::{DriverContext, GenericDriverContext};
pub use helpers::{dtyp_from_port, extract_plugin_args, plugin_arg_defs, register_noop_commands};
pub use plugin_device_support::{ArrayDataHandle, PluginDeviceSupport};
pub use plugin_manager::{PluginInfo, PluginManager};
