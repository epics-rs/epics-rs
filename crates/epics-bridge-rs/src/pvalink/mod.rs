//! `pvalink` — PVA links for EPICS record INP/OUT fields.
//!
//! When a record's INP (or OUT) field carries a link string of the form
//! `@pva://<remote-pv>` (or the legacy `pva://<pv>` form), this module
//! resolves that link to a live PVA client that periodically reads the
//! remote PV (INP) or pushes record output to it (OUT).
//!
//! Mirror of pvxs `ioc/pvalink*.cpp`. Pure Rust, no spvirit_*.
//!
//! ## Usage
//!
//! ```ignore
//! use epics_bridge_rs::pvalink::{PvaLink, PvaLinkConfig};
//!
//! let cfg = PvaLinkConfig::parse("pva://OTHER:IOC:TEMP")?;
//! let link = PvaLink::open(cfg).await?;
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
    db_pvxr_command, pvalink_disable_command, pvalink_enable_command, pvxrefdiff_command,
    register_pvalink_commands,
};
pub use link::{PvaLink, PvaLinkError};
pub use registry::PvaLinkRegistry;
