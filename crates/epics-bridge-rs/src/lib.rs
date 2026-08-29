//! # epics-bridge-rs
//!
//! EPICS protocol bridge/adapter hub.
//!
//! This crate hosts four bridge implementations as feature-gated
//! sub-modules. Each bridge connects EPICS data sources to network
//! protocols (CA or PVA). All four are implemented.
//!
//! ## Sub-modules
//!
//! | Module | Feature | Description |
//! |--------|---------|-------------|
//! | `qsrv` | `qsrv` (default) | Record → pvAccess channels (C++ QSRV equivalent) |
//! | `ca_gateway` | `ca-gateway` | CA fan-out gateway (C++ ca-gateway equivalent) |
//! | `pvalink` | `pvalink` | PVA links for record INP/OUT fields |
//! | `pva_gateway` | `pva-gateway` | PVA-to-PVA proxy (mirrors `pva2pva/p2pApp`) |
//!
//! Enable a bridge with its Cargo feature; `qsrv` is on by default.
//!
//! ## QSRV (Record ↔ PVA bridge)
//!
//! ```text
//! PVA Client ←→ [epics-pva-rs server] ←→ BridgeProvider ←→ PvDatabase
//! ```
//!
//! - `BridgeProvider` implements `ChannelProvider` — the PVA server calls
//!   into it to resolve channel names and create channels.
//! - `BridgeChannel` serves single-record PVs (NTScalar, NTEnum, NTScalarArray).
//! - `GroupChannel` serves multi-record composite PVs from JSON config.
//! - `BridgeMonitor` / `GroupMonitor` bridge `DbSubscription` events to PVA monitor updates.
//!
//! The `ChannelProvider`, `Channel`, and `PvaMonitor` traits are defined in
//! `qsrv`; `qsrv::pva_adapter::QsrvPvStore` bridges them to the native
//! `epics_pva_rs::server_native::ChannelSource` trait so the native PVA
//! server can serve qsrv channels directly.
//!
//! ## ca-gateway (CA fan-out gateway)
//!
//! A pure-Rust port of EPICS `ca-gateway`: a Channel Access proxy that
//! accepts downstream client connections, connects to upstream IOCs,
//! caches PV values and fans out monitor events, applies `.pvlist`
//! access-security rules, and supports regex PV-name aliasing. Channel
//! resolution is lazy on-demand (a downstream search drives an upstream
//! subscription); preloading from a file is an opt-in convenience.
//!
//! ## pvalink (PVA links for record INP/OUT)
//!
//! Resolves record INP/OUT link strings of the form `pva://<remote-pv>`
//! to a live PVA client that periodically reads the remote PV (INP) or
//! pushes record output to it (OUT). Mirrors pvxs `ioc/pvalink*.cpp`.
//!
//! ## pva-gateway (PVA-to-PVA proxy)
//!
//! A PVA-to-PVA proxy mirroring C++ `pva2pva/p2pApp`: one upstream
//! `PvaClient` keeps a per-PV channel cache, and one downstream
//! `PvaServer` forwards GET / PUT / MONITOR / GET_FIELD operations
//! through that cache.
//!
//! # C reference pins
//!
//! Every `file.c:NNN` citation in this crate resolves at the tree and revision
//! below, not at whatever that tree's working copy holds today. These trees are
//! checked out on local branches here and run ahead of their pins.
//!
//! | tree | pinned revision |
//! | --- | --- |
//! | `pvxs` | `1.5.1-42-gb568e93` |
//! | `epics-base` | `R7.0.10` |
//! | `ca-gateway` | `R2-1-3-0-54-g0666f21` |
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

// `build.rs` derives `exec_backend`/`tokio_backend` from this crate's own
// feature set, which is a fourth copy of a rule `epics-base-rs` owns. A copy
// that drifts is worse than no copy: a `runtime::task::spawn` moved off the
// reactor while these gates still said one was there would panic on the first
// CA or PVA socket a gateway task opened. The two views must agree or the
// crate does not compile.
const _: () = assert!(
    epics_base_rs::runtime::task::HAS_TOKIO_REACTOR == cfg!(tokio_backend),
    "epics-bridge-rs and epics-base-rs disagree about the runtime::task backend. \
     Both derive it from EPICS_RS_BUILD_EXEC_BACKEND, so they cannot disagree \
     over what was asked for: one of the two build scripts did not see the \
     variable. Check that both carry `rtems_exec_gate::CANONICAL_DERIVATION`, \
     whose `cargo::rerun-if-env-changed` line is what makes a changed value \
     rebuild this crate"
);

pub mod error;
pub use error::{BridgeError, BridgeResult};

// `EpicsValue` <-> `PvField` conversion helpers. Shared by the QSRV
// bridge and PVA links — both need to translate record values to/from
// pvData. Gated on the consumers that enable `epics-pva-rs` (the only
// extra dependency it uses) so a CA-only build still drops it.
#[cfg(any(feature = "qsrv-core", feature = "pvalink"))]
pub mod convert;

#[cfg(feature = "qsrv-core")]
pub mod qsrv;

#[cfg(feature = "ca-gateway")]
pub mod ca_gateway;

#[cfg(feature = "pvalink")]
pub mod pvalink;

// CA links (`calink`) moved to `epics_ca_rs::calink` (always-on, no
// feature). The qsrv runner installs it via `epics_ca_rs::calink::
// install_calink_resolver`; see `crates/epics-ca-rs/src/calink`.

// The feature alone. `PvaServer` is the reactor-bound half and only
// `pva_gateway::{gateway, multi_gateway}` name it, so the backend predicate
// belongs on those two files and not here: pinning it at the module took the
// cache, the source, the middleware and the control PVs off the reactor-free
// backend with them, and they compile and pass there.
#[cfg(feature = "pva-gateway")]
pub mod pva_gateway;

// `AccessControl` is an `#[async_trait]` trait: re-exported so an out-of-tree
// impl can annotate itself (`#[epics_bridge_rs::async_trait]`) without taking
// its own `async-trait` dependency and risking a version mismatch.
pub use async_trait::async_trait;

// Convenience re-exports for the QSRV bridge (default feature).
// External users can write `epics_bridge_rs::BridgeProvider` directly.
#[cfg(feature = "qsrv-core")]
pub use qsrv::{
    AccessContext, AccessControl, AllowAllAccess, AnyChannel, AnyMonitor, BridgeChannel,
    BridgeMonitor, BridgeProvider, Channel, ChannelProvider, ClientCreds, FieldMapping,
    GroupChannel, GroupMonitor, GroupPvDef, NtType, ProcessMode, PutOptions, PvaMonitor,
};

/// The reactor a unit test spawns on.
///
/// Production code never mints its capability from the ambient executor —
/// that is the whole point of [`epics_base_rs::runtime::task::Reactor`] —
/// but a test *is* the owner of its own executor, so the mint is honest
/// here and the `expect` names the requirement the test already meets.
/// The gate is the union of the four module gates its callers sit behind —
/// `qsrv::pva_adapter`, `pva_gateway`, `ca_gateway` and `pvalink`. Spelled out
/// rather than left at bare `#[cfg(test)]` because a `--no-default-features`
/// build compiles none of the four and then warns that this is never used, and
/// spelled as the union rather than as one feature because each of the four
/// mints a `Reactor` in its own test bodies and any one of them alone is
/// enough to need this.
#[cfg(all(
    test,
    any(
        feature = "qsrv-core",
        feature = "pva-gateway",
        feature = "ca-gateway",
        feature = "pvalink"
    )
))]
pub(crate) fn test_reactor() -> epics_base_rs::runtime::task::Reactor {
    epics_base_rs::runtime::task::Reactor::current()
        .expect("this test body runs inside an executor")
}
