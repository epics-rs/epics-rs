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
//!
//! # C reference pins
//!
//! Every `file.c:NNN` citation in this crate resolves at the tree and revision
//! below, not at whatever that tree's working copy holds today. These trees are
//! checked out on local branches here and run ahead of their pins.
//!
//! | tree | pinned revision |
//! | --- | --- |
//! | `pvxs` | `1.5.1-42-gb568e93`; the TLS/x509 family (`src/ossl.cpp`,
//!   `netcommon.h:133`, `config.cpp:654-660`) exists only on the unmerged
//!   UPSTREAM branch `origin/tls`, cited at `b3a10bf0` (`1.5.2-42-gb3a10bf`)
//!   — not on `fork/tls`, which is a personal fork.  The require-TLS
//!   anti-downgrade family (`tls_disable_plaintext` at `netcommon.h:172`,
//!   its `EPICS_PVA_TLS_OPTIONS` parse at `config.cpp:453-460`, its
//!   enforcement at `src/client.cpp:944`) is on NEITHER: it lives only on the
//!   personal fork branch `fork/fix/tls-peer-identity-and-downgrade`
//!   at `6547f25`, and every citation of it says so |
//! | `epics-base` | `R7.0.10` |
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
//! A row reading *no settled pin* means no revision has been agreed for that
//! tree: say which revision you read, and do not take its `HEAD` for the pin.
//! Citations into non-EPICS sources (libc, RTEMS, `rtems-libbsd`, VxWorks,
//! vendored third-party) are outside this table and carry no pin.

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
// and `EPICS_RS_BUILD_EXEC_BACKEND` — but they compute it independently, so a
// build in which one of the two scripts did not see the variable would give
// `runtime::task::spawn` a reactor-free backend while this crate still compiled
// the reactor-backed UDP SEARCH transport in and selected it. That is exactly
// the configuration measured as a boot panic, and it is the one state the
// two-variant `client_native::search_engine::SearchTransport` cannot rule out
// on its own.
//
// So it is ruled out here instead: the two views must agree or the crate does
// not compile.
const _: () = assert!(
    epics_base_rs::runtime::task::HAS_TOKIO_REACTOR == cfg!(tokio_backend),
    "epics-pva-rs and epics-base-rs disagree about the runtime::task backend. \
     Both derive it from EPICS_RS_BUILD_EXEC_BACKEND, so they cannot disagree \
     over what was asked for: one of the two build scripts did not see the \
     variable. Check that both carry \
     `rtems_exec_gate::CANONICAL_DERIVATION`, whose \
     `cargo::rerun-if-env-changed` line is what makes a changed value rebuild \
     this crate"
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

/// The reactor a unit test spawns on.
///
/// Production code never mints its capability from the ambient executor —
/// that is the whole point of [`epics_base_rs::runtime::task::Reactor`] —
/// but a test *is* the owner of its own executor, so the mint is honest
/// here and the `expect` names the requirement the test already meets.
#[cfg(test)]
pub(crate) fn test_reactor() -> epics_base_rs::runtime::task::Reactor {
    epics_base_rs::runtime::task::Reactor::current()
        .expect("this test body runs inside an executor")
}
