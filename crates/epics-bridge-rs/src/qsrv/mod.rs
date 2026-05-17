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
pub mod iocsh;
pub mod monitor;
pub mod provider;
#[cfg(feature = "qsrv")]
pub mod pva_adapter;
pub mod pvif;

pub use channel::{BridgeChannel, ProcessMode, PutOptions};
pub use group::{AnyMonitor, GroupChannel, GroupMonitor};
pub use group_config::GroupPvDef;
pub use monitor::BridgeMonitor;
pub use provider::{
    AccessContext, AccessControl, AcfAccessControl, AllowAllAccess, AnyChannel, BridgeProvider,
    Channel, ChannelProvider, PvaMonitor,
};
#[cfg(feature = "qsrv")]
pub use pva_adapter::{
    PvaPvHandle, QsrvPvStore, register_pva_pv_global, run_ca_pva_qsrv_ioc, take_registered_pva_pvs,
};
pub use pvif::{FieldMapping, NtType};
