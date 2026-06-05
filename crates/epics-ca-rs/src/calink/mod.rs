//! `calink` — Channel Access links for EPICS record INP/OUT fields.
//!
//! When a record's INP / OUT / DOL / STPL link field carries a
//! `ca://<pv>` string, or a legacy `<rec.field> CA` string (the bare
//! ` CA` modifier — C `dbStaticLib.c:2372` `pvlOptCA`), this module
//! resolves that link to a live CA client whose monitor keeps a cached
//! snapshot of the remote PV.
//!
//! This is the CA-side counterpart of the bridge `pvalink` module. It
//! mirrors C `dbCa.c` / `dbCaLink`: each CA link attaches one CA channel
//! and one subscription; `dbCaGetLink` (`dbCa.c:448`) is served from the
//! cached value populated by the monitor `eventCallback`
//! (`dbCa.c:925`) — a CA link is **monitor-backed**, served from
//! cache, never a synchronous per-read fetch.
//!
//! ## Why this lives in `epics-ca-rs`
//!
//! A CA-link resolver needs both halves: the database-side
//! [`epics_base_rs::server::database::LinkSet`] / `PvDatabase` AND a live
//! CA client ([`crate::client::CaClient`]). `epics-ca-rs` already depends
//! on `epics-base-rs` (`epics-ca-rs → epics-base-rs`), so it is the
//! natural home — both halves are in scope with no new dependency.
//! `epics-base-rs` itself cannot host this: a `base → ca` dependency
//! would be a cycle. The `LinkSet` trait is the seam that lets the
//! database resolve CA links without `epics-base-rs` depending on the CA
//! crate — so simply enabling `epics-ca-rs` provides CA-link resolution,
//! no separate feature opt-in.
//!
//! ## Usage
//!
//! ```ignore
//! use epics_ca_rs::calink::install_calink_resolver;
//!
//! let resolver = install_calink_resolver(&db, tokio::runtime::Handle::current()).await?;
//! // Records whose INP is `ca://OTHER:IOC:TEMP` (or `OTHER:IOC:TEMP CA`)
//! // now resolve through the monitor-backed cache.
//! ```

mod iocsh;
mod resolver;

pub use iocsh::{ca_caxr_command, db_dbcaxr_command, register_calink_commands};
pub use resolver::{CaLink, CaLinkError, CaLinkResolver, install_calink_resolver};
