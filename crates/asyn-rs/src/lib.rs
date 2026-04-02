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
pub(crate) mod exception;
pub mod interfaces;
pub(crate) mod interpose;
pub mod interrupt;
pub mod manager;
pub mod param;
pub mod port;
pub(crate) mod port_actor;
pub mod port_handle;
pub(crate) mod protocol;
pub mod request;
pub mod runtime;
pub mod sync_io;
pub mod trace;
pub(crate) mod transport;
pub mod user;

#[cfg(feature = "epics")]
pub mod adapter;
#[cfg(feature = "epics")]
pub mod asyn_record;
