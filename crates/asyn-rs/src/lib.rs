#![allow(
    unused_imports,
    clippy::approx_constant,
    clippy::collapsible_if,
    clippy::derivable_impls,
    clippy::if_same_then_else,
    clippy::manual_range_contains,
    clippy::single_match,
    clippy::unnecessary_map_or
)]

pub mod drivers;
pub mod error;
pub(crate) mod escape;
// Public: `PortManager::exception_manager` / `PortServices::exceptions` hand
// out an `Arc<ExceptionManager>`, and a caller registering an exception
// callback needs to name `AsynException` to match on it.
pub mod exception;
pub mod interfaces;
pub mod interpose;
pub mod interrupt;
pub mod manager;
pub mod param;
pub mod port;
pub(crate) mod port_actor;
pub mod port_handle;
pub(crate) mod protocol;
pub mod registry;
pub mod request;
pub mod runtime;
pub mod services;
pub mod sync_io;
pub mod timestamp;
pub mod trace;
pub(crate) mod transport;
pub mod user;

#[cfg(feature = "epics")]
pub mod adapter;
#[cfg(feature = "epics")]
pub mod asyn_record;
/// The asyn device-support DTYP menus, generated from the vendored asyn `.dbd`
/// by `tools/dbd-codegen` — the same path base and every other downstream crate
/// use. `crate::adapter::register_asyn_device_menus` hands each entry to base's
/// `register_device_menu` so a client reading e.g. `mbbo.DTYP` sees the asyn
/// choices a C fat softIoc lists. Gated on `epics` because it names
/// `epics_base_rs` types.
#[cfg(feature = "epics")]
pub mod dbd_generated;
#[cfg(feature = "epics")]
pub mod iocsh;
