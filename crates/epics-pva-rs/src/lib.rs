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
// The native PVA client — connection pool, UDP search engine, per-operation
// tasks. Behind the `client` feature (ON by default) so a server-only build
// (design doc §9 phase 6) can drop the client I/O surface. The wire decoder
// the server also needs is NOT in here; it is `crate::decode`.
#[cfg(feature = "client")]
pub mod client;
#[cfg(feature = "client")]
pub mod client_native;
pub mod codec;
pub mod config;
pub mod decode;
pub mod error;
pub mod format;
pub(crate) mod leaf_convert;
pub mod log;
pub mod nt;
pub mod peer_buf;
pub mod proto;
pub mod pv_request;
pub mod pvdata;
pub mod server;
pub mod server_native;
pub mod service;
pub mod util;

pub use error::{PvaError, PvaResult};

// Pins this crate's `exec_backend`/`tokio_backend` decision (`build.rs`) to
// `epics-base-rs`'s.
//
// Both scripts compute the same rule from the same two inputs — the target OS
// and their own `rtems-exec-model` feature — and cargo features unify per
// *package*, so a build that turned on `epics-base-rs/rtems-exec-model`
// without this crate's own `rtems-exec-model` would give `runtime::task::spawn`
// a reactor-free backend while this crate still compiled the reactor-backed
// UDP SEARCH transport in and selected it. That is exactly the configuration
// `doc/calink-rtems-design.md` §10.10 item 2 measured as a boot panic, and it
// is the one state the two-variant `client_native::search_engine::
// SearchTransport` cannot rule out on its own.
//
// So it is ruled out here instead: the two views must agree or the crate does
// not compile.
const _: () = assert!(
    epics_base_rs::runtime::task::HAS_TOKIO_REACTOR == cfg!(tokio_backend),
    "epics-pva-rs and epics-base-rs disagree about the runtime::task backend: \
     enable `epics-pva-rs/rtems-exec-model` rather than \
     `epics-base-rs/rtems-exec-model` alone"
);

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
