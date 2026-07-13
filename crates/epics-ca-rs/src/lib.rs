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

pub mod audit;
/// CA links for record INP/OUT fields — resolves ` CA`-modified /
/// `ca://` record link fields to a live CA client (monitor-backed
/// cache). Mirrors C `dbCa.c` / `dbCaLink`. Always compiled: having
/// `epics-ca-rs` is enough to resolve CA links, no feature opt-in.
pub mod calink;
pub mod cap_token;
pub(crate) mod channel;
pub mod chaos;
pub mod cli;
pub mod client;
pub mod copt;
pub mod discovery;
pub mod hostname;
pub mod observability;
pub mod protocol;
pub mod repeater;
pub mod replay;
pub mod server;
pub mod tls;

// Re-export commonly used types from epics-base-rs for convenience
pub use epics_base_rs::error::{CaError, CaResult};
pub use epics_base_rs::runtime;
pub use epics_base_rs::types::{DbFieldType, EpicsValue, PvString};
