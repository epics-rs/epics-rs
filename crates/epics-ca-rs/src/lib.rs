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

// The CA client, the discovery stack, and the CA-link resolver.
//
// The client and everything over it (`channel`, `calink`, the `cli`/`copt`
// tool support) are selected by the `client-core` FEATURE rather than by the
// target, because which of them a build gets is a choice a build makes — a
// record link needs the circuit, the search engine and the resolver, and
// needs none of the UDP discovery stack (`doc/calink-rtems-design.md` §2.1).
// `default = ["client"]` keeps every hosted consumer on the full set; an
// RTEMS image selects `client-core` (`scripts/rtems-check.sh`).
//
// `discovery`, `repeater` and `hostname` stay gated on the TARGET, not on a
// feature: each is host-only for a reason that is a fact about the platform
// (`mdns-sd`/`hickory` do not build for RTEMS; `getnameinfo` has no newlib
// backing), and each has a consumer outside the client — `discovery` backs
// the CA server's mDNS/DNS announce, `repeater` backs `ca-repeater-rs`. What
// the `client` feature owns is the client's *references* to them.
pub mod audit;
/// CA links for record INP/OUT fields — resolves ` CA`-modified /
/// `ca://` record link fields to a live CA client (monitor-backed
/// cache). Mirrors C `dbCa.c` / `dbCaLink`. Compiled whenever the client
/// is: having `epics-ca-rs` with its default features is enough to
/// resolve CA links, no separate opt-in.
#[cfg(feature = "client-core")]
pub mod calink;
pub mod cap_token;
// CA client channel state (`AccessRights`, id allocators); used only by
// `client`.
#[cfg(feature = "client-core")]
pub(crate) mod channel;
pub mod chaos;
// CA client-tool option/format helpers. They name `client` items, so they
// follow it; still host-only on top of that, because their consumers are the
// host CLI binaries.
#[cfg(all(feature = "client-core", not(target_os = "rtems")))]
pub mod cli;
#[cfg(feature = "client-core")]
pub mod client;
// CA client-tool (`caget`/`caput`/`cainfo`) argument parsing; it references
// `cli::IntStyle` and backs only the host client binaries.
#[cfg(all(feature = "client-core", not(target_os = "rtems")))]
pub mod copt;
#[cfg(not(target_os = "rtems"))]
pub mod discovery;
pub mod estdlib;
// Reverse-DNS (`getnameinfo` via `socket2::SockAddr`) for the CA client's
// peer-name cache. Its only consumer is the client's `peer_display_name` /
// `peer_resolved_name`, so it follows the `client` feature as well as the
// target: `client-core` names a peer by its dotted address, which is C's own
// answer for an address with no PTR record.
#[cfg(all(feature = "client", not(target_os = "rtems")))]
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
