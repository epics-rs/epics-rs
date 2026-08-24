#![allow(
    clippy::approx_constant,
    clippy::collapsible_match,
    clippy::collapsible_if,
    clippy::derivable_impls,
    clippy::field_reassign_with_default,
    clippy::implicit_saturating_sub,
    clippy::io_other_error,
    clippy::items_after_test_module,
    clippy::manual_is_multiple_of,
    clippy::manual_range_contains,
    clippy::manual_strip,
    clippy::map_entry,
    clippy::needless_range_loop,
    clippy::new_without_default,
    clippy::redundant_closure,
    clippy::should_implement_trait,
    clippy::single_match,
    clippy::type_complexity,
    clippy::unnecessary_cast,
    clippy::unnecessary_map_or,
    clippy::useless_conversion
)]

// `epics-macros-rs` attribute expansions refer to this crate as
// `::epics_base_rs` (the spelling that is correct in downstream crates and in
// this package's own integration tests/bins, where `crate` would name the
// wrong crate). This alias makes the same spelling resolve inside the library
// target itself, so one expansion works everywhere.
extern crate self as epics_base_rs;

/// The upstream EPICS Base release this crate ports — C's
/// `EPICS_VERSION_FULL`.
///
/// Not written here: [`runtime::version`] is generated from the vendored
/// `configure/CONFIG_BASE_VERSION` the same way C generates
/// `epicsVersion.h`, so the next upstream bump is a spec edit plus a
/// regeneration, not a literal somebody has to remember to change.
pub use runtime::version::EPICS_VERSION_FULL as EPICS_BASE_VERSION;

/// `LinkSet` is an `#[async_trait]` trait: re-exported so an out-of-tree lset
/// can annotate its impl without taking its own `async-trait` dependency.
pub use async_trait::async_trait;

pub mod calc;
pub mod error;
pub mod json5;
pub mod reference;
// The async UDP net stack (`tokio::net` + `socket2` + `if-addrs`) is host-only:
// its deps do not build for RTEMS, and the RTEMS CA server uses the separate
// S1 raw-libc socket driver, not those modules. The gate now sits on the
// socket-bearing submodules rather than on `net` itself, so the wire constants
// beside them (`ORIGIN_TAG_MCAST_GROUP`) stay reachable from the protocol code
// that has to embed them on RTEMS too — see the module doc.
//
// `net` and `runtime` now live in `epics-libcom-rs` (issue #55) so a consumer
// can take the socket/concurrency layer without the record system. They are
// re-exported here under their original names rather than left to callers to
// depend on directly: `epics_base_rs::net::…` and `epics_base_rs::runtime::…`
// are the paths every downstream crate and every module below already spells,
// and the re-export keeps `crate::net::…` / `crate::runtime::…` valid inside
// this crate too, so the split cost zero call-site edits.
pub use epics_libcom_rs::{net, runtime};
pub mod server;
pub mod types;

// The `exec_backend` / `tokio_backend` cfg is derived twice — once by
// `epics-libcom-rs`'s build script for the task seam, once by this crate's
// for `server::scan` above it — because a dependency's cfg is not visible
// here. This pins the two copies together: if `rtems-exec-model` ever stops
// forwarding to `epics-libcom-rs/rtems-exec-model`, the workspace would
// otherwise compile with the record system on one backend and the seam
// beneath it on the other, which is a runtime symptom (no reactor, or two)
// rather than a build error. Now it is a build error.
const _: () = assert!(
    epics_libcom_rs::EXEC_BACKEND == cfg!(exec_backend),
    "epics-base-rs and epics-libcom-rs disagree about the task backend — \
     check that `rtems-exec-model` still forwards to `epics-libcom-rs`"
);

pub use epics_macros_rs::epics_main;
pub use epics_macros_rs::epics_test;

#[doc(hidden)]
pub use tokio as __tokio;
