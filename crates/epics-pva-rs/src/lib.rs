#![allow(
    clippy::collapsible_if,
    clippy::map_entry,
    clippy::new_without_default,
    clippy::redundant_closure,
    clippy::single_match,
    clippy::type_complexity,
    clippy::unnecessary_cast
)]

//! EPICS pvAccess protocol — client and server.
//!
//! This crate provides the pvAccess wire protocol implementation,
//! separated from the core IOC infrastructure in `epics-base-rs`.

pub mod auth;
pub mod cli;
pub mod client;
pub mod client_native;
pub mod codec;
pub mod config;
pub mod error;
pub mod format;
pub(crate) mod leaf_convert;
pub mod log;
pub mod nt;
pub mod proto;
pub mod pv_request;
pub mod pvdata;
pub mod server;
pub mod server_native;
pub mod service;
pub mod util;

pub use error::{PvaError, PvaResult};

// Re-export commonly used types from epics-base-rs
pub use epics_base_rs::types::{DbFieldType, EpicsValue};

// Re-export commonly used pvData types so downstream callers can pull them
// from the crate root.
pub use pvdata::{FieldDesc, PvField, PvStructure, ScalarType, ScalarValue};

/// Runtime version packed as `(major << 24) | (minor << 16) | (patch << 8)`.
/// Mirrors pvxs `version_int()` (util.cpp:69). The low byte is reserved
/// for build metadata (always 0 here). Useful for capability-gating
/// against a specific minimum runtime version.
pub const fn version_int() -> u32 {
    let major = parse_u32(env!("CARGO_PKG_VERSION_MAJOR"));
    let minor = parse_u32(env!("CARGO_PKG_VERSION_MINOR"));
    let patch = parse_u32(env!("CARGO_PKG_VERSION_PATCH"));
    (major << 24) | (minor << 16) | (patch << 8)
}

/// Runtime version string — `env!("CARGO_PKG_VERSION")` re-exported for
/// API discoverability alongside [`version_int`].
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

const fn parse_u32(s: &str) -> u32 {
    let mut out: u32 = 0;
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if b < b'0' || b > b'9' {
            break;
        }
        out = out * 10 + (b - b'0') as u32;
        i += 1;
    }
    out
}
