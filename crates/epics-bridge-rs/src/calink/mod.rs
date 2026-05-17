//! `calink` — Channel Access links for EPICS record INP/OUT fields.
//!
//! When a record's INP / OUT / DOL / STPL link field carries a
//! `ca://<pv>` string, or a legacy `<rec.field> CA` string (the bare
//! ` CA` modifier — C `dbStaticLib.c:2372` `pvlOptCA`), this module
//! resolves that link to a live CA client whose monitor keeps a cached
//! snapshot of the remote PV.
//!
//! This is the CA-side counterpart of [`crate::pvalink`]. It mirrors C
//! `dbCa.c` / `dbCaLink`: each CA link attaches one CA channel and one
//! subscription; `dbCaGetLink` (`dbCa.c:448`) is served from the
//! cached value populated by the monitor `eventCallback`
//! (`dbCa.c:925`) — a CA link is **monitor-backed**, served from
//! cache, never a synchronous per-read fetch.
//!
//! `epics-base-rs` cannot host this itself: its dependency on
//! `epics-ca-rs` is a dev-dependency only (the non-dev arrow runs
//! `epics-ca-rs → epics-base-rs`), so a CA client cannot be wired into
//! the database crate without a dependency cycle. The
//! [`epics_base_rs::server::database::LinkSet`] trait is the seam
//! designed to break that cycle — exactly as [`crate::pvalink`] does
//! for PVA.
//!
//! ## Usage
//!
//! ```ignore
//! use epics_bridge_rs::calink::install_calink_resolver;
//!
//! let resolver = install_calink_resolver(&db, tokio::runtime::Handle::current()).await?;
//! // Records whose INP is `ca://OTHER:IOC:TEMP` (or `OTHER:IOC:TEMP CA`)
//! // now resolve through the monitor-backed cache.
//! ```

mod iocsh;
mod resolver;

pub use iocsh::{ca_caxr_command, db_dbcaxr_command, register_calink_commands};
pub use resolver::{CaLink, CaLinkError, CaLinkResolver, install_calink_resolver};
