//! # C reference pins
//!
//! Every `file.c:NNN` citation in this crate resolves at the tree and revision
//! below, not at whatever that tree's working copy holds today. These trees are
//! checked out on local branches here and run ahead of their pins.
//!
//! | tree | pinned revision |
//! | --- | --- |
//! | `epics-base` | `R7.0.10` |
//! | `calc` | `R3-7-5-49-gf207871` |
//! | `pvxs` | `1.5.1-42-gb568e93` |
//! | `busy` | `R1-7-4-6-g2dfe92d` |
//! | `asyn` | `R4-45-19-ge2a281e2` |
//! | `std` | `R3-6-4` |
//! | `motor` | `R7-4-5-g78b474cd` |
//! | `scaler` | `4.1` |
//! | `ca-gateway` | `R2-1-3-0-54-g0666f21` |
//! | `optics` | `R2-14-15-g3def19d` |
//! | `mca` | `687d563` (tree carries no tags) |
//!
//! **Resolve by symbol at the pin; the line is a hint.** Find the named
//! function, struct, macro or field first, and treat the line number as a hint
//! that has to land inside that construct. Three cases follow:
//!
//! 1. Construct at the pin, line lands in it — the citation is exact. A
//!    reference checkout ahead of the pin will disagree; that disagreement is
//!    the checkout's, not the citation's.
//! 2. Construct at the pin, line lands outside it — line drift. Keep the
//!    symbol and move the line to the pin's.
//! 3. Construct absent at the pin — the citation means code added after it,
//!    and is NOT moved onto the pin, where it would point at lines that do not
//!    exist. It names the revision it means inline, beside the line span: the
//!    upstream PR and commit, and that both are later than the pin this table
//!    gives. `epics-libcom-rs` already carries that form.
//!
//! Every pin above passes `git merge-base --is-ancestor <pin> origin/<default>`
//! in its own tree, which is the test a pin has to meet. A `git describe`
//! string names an exact commit and is worth as much as a tag; what
//! disqualifies a revision is being reachable only from a fork branch or an
//! unmerged PR, because then it names nothing a reader outside this workspace
//! can fetch.
//!
//! Resolve each citation on its own. One sentence can cite two lines that are
//! right at different revisions, and a check run at either revision then
//! reports a single tidy error while vouching for the very citation the other
//! condemns.
//!
//! The `epics-base` tree on this machine is not the pin: it is checked out on
//! a local branch at `R7.0.10-146-g8f5015b66`, R7.0.10 plus 146 commits plus
//! the unmerged PR #944, and the drift is not one offset. `iocsh.cpp` is 1616
//! lines at the pin and 1623 there, running level below `Tokenize`, +3 across
//! the readline block and +6 from `iocshRegisterVariable` on, so a citation
//! carried over from that checkout lands in a neighbouring construct rather
//! than out of range.
//!
//! A row reading *no settled pin* means no revision has been agreed for that
//! tree: say which revision you read, and do not take its `HEAD` for the pin.
//! Citations into non-EPICS sources (libc, RTEMS, `rtems-libbsd`, VxWorks,
//! vendored third-party) are outside this table and carry no pin.

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
// here. This pins the two copies together: if either build script misses
// EPICS_RS_BUILD_EXEC_BACKEND, the workspace would otherwise compile with the
// record system on one backend and the seam beneath it on the other, which is
// a runtime symptom (no reactor, or two) rather than a build error. Now it is
// a build error.
const _: () = assert!(
    epics_libcom_rs::EXEC_BACKEND == cfg!(exec_backend),
    "epics-base-rs and epics-libcom-rs disagree about the task backend — \
     one of the two build scripts did not see EPICS_RS_BUILD_EXEC_BACKEND; \
     check that both carry `rtems_exec_gate::CANONICAL_DERIVATION`"
);

pub use epics_macros_rs::epics_main;
pub use epics_macros_rs::epics_test;

#[doc(hidden)]
pub use tokio as __tokio;
