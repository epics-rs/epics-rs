//! # qsrv — Record ↔ pvAccess bridge (C++ QSRV equivalent)
//!
//! Exposes EPICS database records as pvAccess channels (NTScalar, NTEnum,
//! NTScalarArray, Group PV).
//!
//! Corresponds to C++ EPICS QSRV (`modules/pva2pva/pdbApp/`). Translates
//! between `epics-base-rs` record state and `epics-pva-rs` PVA data
//! structures.
//!
//! ## Architecture
//!
//! ```text
//! PVA Client ←→ [epics-pva-rs server] ←→ BridgeProvider ←→ PvDatabase
//! ```
//!
//! - [`BridgeProvider`] implements [`ChannelProvider`] — the PVA server calls
//!   into it to resolve channel names and create channels.
//! - [`BridgeChannel`] serves single-record PVs (NTScalar, NTEnum, NTScalarArray).
//! - [`GroupChannel`] serves multi-record composite PVs from JSON config.
//! - [`BridgeMonitor`] / [`GroupMonitor`] bridge `DbSubscription` events to PVA monitor updates.
//!
//! The `ChannelProvider`, `Channel`, and `PvaMonitor` traits are defined in
//! this module. [`pva_adapter::QsrvPvStore`] bridges them to the native
//! [`epics_pva_rs::server_native::ChannelSource`] trait so the native PVA
//! server can serve qsrv channels directly.

pub mod channel;
pub mod group;
pub mod group_config;
pub(crate) mod group_pump;
pub mod iocsh;
pub mod monitor;
pub mod provider;
pub(crate) mod put_status;
#[cfg(feature = "qsrv-core")]
pub mod pva_adapter;
pub mod pvif;
pub(crate) mod trap_write;

pub use channel::{BridgeChannel, ProcessMode, PutOptions};
pub use group::{AnyMonitor, GroupChannel, GroupMonitor};
pub use group_config::GroupPvDef;
pub use monitor::BridgeMonitor;
pub use provider::{
    AccessContext, AccessControl, AcfAccessControl, AllowAllAccess, AnyChannel, BridgeProvider,
    Channel, ChannelProvider, ClientCreds, PvaMonitor, WriteGrant,
};
#[cfg(feature = "qsrv-core")]
pub use pva_adapter::{
    PvaPvHandle, QsrvMount, QsrvPvStore, build_qsrv_mount, pvalink_link_set_install,
    register_pva_pv_global, take_registered_pva_pvs,
};
// The host dual-protocol runner: gated with its definition, and the predicate
// must match it exactly — `qsrv` (not `qsrv-core`) because it needs
// `epics-ca-rs`, and `not(epics_embedded_target)` because both servers it
// starts are embedded-target-gated in their own crates. See the definition
// for the full reasoning.
#[cfg(all(feature = "qsrv", not(epics_embedded_target)))]
pub use pva_adapter::run_ca_pva_qsrv_ioc;
pub use pvif::{FieldMapping, NtType};
