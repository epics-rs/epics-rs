#![allow(
    clippy::collapsible_if,
    clippy::map_entry,
    clippy::io_other_error,
    clippy::new_without_default,
    clippy::redundant_closure,
    clippy::single_match,
    clippy::type_complexity,
    clippy::unnecessary_cast
)]

//! EPICS Channel Access protocol — client and server.
//!
//! This crate provides the CA wire protocol implementation,
//! separated from the core IOC infrastructure in `epics-base-rs`.

pub(crate) mod channel;
pub mod client;
pub mod protocol;
pub mod repeater;
pub mod server;

// Re-export commonly used types from epics-base-rs for convenience
pub use epics_base_rs::error::{CaError, CaResult};
pub use epics_base_rs::runtime;
pub use epics_base_rs::types::{DbFieldType, EpicsValue};
