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
