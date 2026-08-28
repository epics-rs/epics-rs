//! IOC support for areaDetector plugins.
//!
//! Provides reusable infrastructure for building IOC binaries with
//! areaDetector plugins:
//!
//! - [`DriverContext`] trait: abstraction for driver runtime (pool, downstream wiring)
//! - [`PluginManager`]: manages plugin lifecycle, port registration, wiring
//! - Helper functions for C-compatible plugin configure commands

mod driver_context;
mod helpers;
mod plugin_manager;

pub use driver_context::{DriverContext, GenericDriverContext};
pub use helpers::{
    attr_arg_defs, dtyp_from_port, extract_plugin_args, max_threads_arg, plugin_arg_defs,
    plugin_arg_defs_with_count, register_noop_commands,
};
pub use plugin_manager::PluginManager;
