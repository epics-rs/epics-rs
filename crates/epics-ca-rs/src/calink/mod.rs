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
//! A CA-serving IOC built on [`epics_base_rs::server::ioc_app::IocApplication`]
//! wires the link set once, at construction, via the base
//! `register_link_set_installer` seam. The installer fires at the
//! iocInit `AfterCaLinkInit` hook — BEFORE `setup_cp_links` warms Passive
//! CP holders — so CA links (including a Passive CP/CPP holder's source)
//! resolve with no further setup:
//!
//! ```ignore
//! use epics_ca_rs::calink::calink_link_set_install;
//!
//! IocApplication::new()
//!     .startup_script("st.cmd")
//!     .register_link_set_installer(calink_link_set_install)
//!     .run(epics_ca_rs::server::run_ca_ioc)
//!     .await
//! ```
//!
//! The lower-level [`crate::calink::install_calink_resolver`] is also available for
//! callers that drive their own database assembly (it must run before
//! `setup_cp_links`). The shared CA client is created lazily on the
//! first CA link open, so an IOC with no CA links never spins one up.

mod iocsh;
mod resolver;

use std::sync::Arc;

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::iocsh::registry::CommandDef;

pub use iocsh::{ca_caxr_command, db_dbcaxr_command, register_calink_commands};
pub use resolver::{CaLink, CaLinkError, CaLinkResolver, install_calink_resolver};

/// Async link-set installer for
/// [`epics_base_rs::server::ioc_app::IocApplication::register_link_set_installer`].
///
/// Installs the `"ca"` link set on `db` and returns the `caxr` /
/// `dbcaxr` iocsh commands. Registered at IOC construction, it is fired
/// by `IocApplication::run` at the `AfterCaLinkInit` hook — before
/// `setup_cp_links` warms Passive CP holders — so this is all a
/// CA-serving IOC needs to make CA links resolve: no feature opt-in, no
/// manual [`CaLinkResolver::open`] calls. The shared CA client is
/// created lazily on the first link open.
pub async fn calink_link_set_install(db: Arc<PvDatabase>) -> Vec<CommandDef> {
    let resolver = install_calink_resolver(&db).await;
    register_calink_commands(resolver)
}
