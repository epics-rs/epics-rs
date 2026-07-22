// A `pub` type that no downstream path can name is a public API hole: a
// caller receives the value and cannot write its type, so they cannot store
// it in a struct, name it in a signature, or implement a trait over it. Three
// of these were live in this crate and one of them — `ExceptionSite` on the
// public `CaException` — was found only because a rustdoc link happened to
// point at it. This lint finds the population instead of the sample, and the
// crate's `clippy -D warnings` gate turns it into a build failure.
#![warn(unnameable_types)]
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

// The async CA client, discovery, repeater, and CA-link resolver are the
// `tokio::net` host-only front-end: their deps (`tokio` net feature → `mio`,
// `socket2`, `if-addrs`) do not build for RTEMS. The RTEMS build serves CA
// only through the `std::net` blocking server driver (`server::blocking`);
// client-side connectivity and the discovery stack are a later increment.
// Gated out for the RTEMS target (armv7-rtems-eabihf).
pub mod audit;
/// CA links for record INP/OUT fields — resolves ` CA`-modified /
/// `ca://` record link fields to a live CA client (monitor-backed
/// cache). Mirrors C `dbCa.c` / `dbCaLink`. Always compiled on hosted:
/// having `epics-ca-rs` is enough to resolve CA links, no feature
/// opt-in. Host-only (it drives a live CA client, `tokio::net`).
#[cfg(not(target_os = "rtems"))]
pub mod calink;
pub mod cap_token;
// CA client channel state (`AccessRights`, id allocators); used only by the
// host-only `client`. Host-only.
#[cfg(not(target_os = "rtems"))]
pub(crate) mod channel;
pub mod chaos;
#[cfg(not(target_os = "rtems"))]
pub mod cli;
#[cfg(not(target_os = "rtems"))]
pub mod client;
// CA client-tool (`caget`/`caput`/`cainfo`) argument parsing; it references
// `cli::IntStyle` and backs only the host client binaries. Host-only.
#[cfg(not(target_os = "rtems"))]
pub mod copt;
#[cfg(not(target_os = "rtems"))]
pub mod discovery;
pub mod estdlib;
// Reverse-DNS (`getnameinfo` via `socket2::SockAddr`) for the CA client's
// peer-name cache; only the client uses it. Host-only.
#[cfg(not(target_os = "rtems"))]
pub mod hostname;
pub(crate) mod iocinf;
pub mod observability;
pub mod protocol;
#[cfg(not(target_os = "rtems"))]
pub mod repeater;
pub mod replay;
pub mod server;
pub mod tls;

// Re-export commonly used types from epics-base-rs for convenience
pub use epics_base_rs::error::{CaError, CaResult};
pub use epics_base_rs::runtime;
pub use epics_base_rs::types::{DbFieldType, EpicsValue, PvString};
