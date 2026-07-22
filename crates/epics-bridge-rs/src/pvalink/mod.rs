//! `pvalink` — PVA links for EPICS record INP/OUT fields.
//!
//! When a record's INP (or OUT) field carries a link string of the form
//! `@pva://<remote-pv>` (or the legacy `pva://<pv>` form), this module
//! resolves that link to a live PVA client that periodically reads the
//! remote PV (INP) or pushes record output to it (OUT).
//!
//! Mirror of pvxs `ioc/pvalink*.cpp`. Pure Rust.
//!
//! ## Usage
//!
//! The IOC-wide [`PvaClient`](epics_pva_rs::client::PvaClient) lives on
//! the [`PvaLinkRegistry`] (pvxs `linkGlobal->provider_remote`), so links
//! are opened through it rather than constructed directly — every link
//! then shares one connection pool and one search engine.
//!
//! ```ignore
//! use epics_bridge_rs::pvalink::{LinkDirection, PvaLinkConfig, PvaLinkRegistry};
//!
//! let registry = PvaLinkRegistry::new();
//! let cfg = PvaLinkConfig::parse("pva://OTHER:IOC:TEMP", LinkDirection::Inp)?;
//! let link = registry.get_or_open(cfg).await?;
//! let value = link.read().await?;
//! ```

mod config;
mod integration;
mod iocsh;
mod link;
mod registry;

pub use config::{LinkDirection, PvaLinkConfig, PvaLinkParseError, SevrMode};
pub use integration::{PvaLinkResolver, install_pvalink_resolver};
pub use iocsh::{
    db_pvxr_command, pvalink_disable_command, pvalink_enable_command, pvalinkrefdiff_command,
    register_pvalink_commands,
};
pub use link::{PvaLink, PvaLinkError};
pub use registry::PvaLinkRegistry;
