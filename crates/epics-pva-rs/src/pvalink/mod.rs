//! `pvalink` — PVA links for EPICS record INP/OUT fields.
//!
//! When a record's INP (or OUT) field carries a pvxs JSON link
//! `{pva:"<remote-pv>"}` / `{pva:{pv:…}}`, this module resolves that link
//! to a live PVA client that periodically reads the remote PV (INP) or
//! pushes record output to it (OUT).
//!
//! Not `@pva://…`: a leading `@` is the INST_IO sigil, so
//! `try_parse_hw_link` (`epics-base-rs` `link.rs:1074-1086`) claims the
//! field and returns `ParsedLink::Hw`. `iocInit`
//! then refuses it — *"can't initialize link type CONSTANT with
//! \"@pva://UPSTREAM:AI CP\" (type INST_IO)"* — because a soft record's
//! device support declares CONSTANT. Measured on the target.
//! [`config::PvaLinkConfig::parse`] refuses the `@` prefix
//! outright — one rule with the record loader, not a laxer second one
//! on a path no record can reach.
//!
//! Mirror of pvxs `ioc/pvalink*.cpp`. Pure Rust.
//!
//! ## Usage
//!
//! The IOC-wide [`PvaClient`](crate::client::PvaClient) lives on
//! the [`PvaLinkRegistry`] (pvxs `linkGlobal->provider_remote`), so every
//! link shares one connection pool and one search engine. Links are opened
//! by the [`PvaLinkResolver`] that [`install_pvalink_resolver`] registers as
//! the `pva` lset; the registry's open is private to its `local` gate.

mod config;
mod integration;
mod iocsh;
mod link;
mod registry;

pub use config::{LinkDirection, PvaLinkConfig, PvaLinkParseError, SevrMode};
pub use integration::{PvaLinkResolver, install_pvalink_resolver};
pub use iocsh::{
    db_pvxr_command, pvalink_disable_command, pvalink_enable_command, pvalink_enable_commands,
    pvalinkrefdiff_command, register_pvalink_commands,
};
pub use link::{PvaLink, PvaLinkError};
pub use registry::PvaLinkRegistry;

use std::sync::{Arc, OnceLock};

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::ioc_app::IocApplication;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::iocsh::registry::CommandDef;

/// pvxs `lsetPVX`: installs the `pva` link set on `db` and returns its iocsh
/// commands, with pvxs `pvalink_enable()`'s when
/// [`qsrv2_enabled`](crate::server::iocsh::qsrv2_enabled), the `enable2()`
/// decision, which gates only those (`ioc/iochooks.cpp:503-507`).
///
/// Run at `AfterCaLinkInit`, before `setup_cp_links` and PINI, so a pva CP
/// link's channel is already connecting while iocInit runs, as pvxs opens
/// pvalink channels at `initHookAfterIocBuilt` (`linkGlobal_t::init`). PVA
/// links take no part in `dbCaRun`'s wait: pvxs pvalink never blocks iocInit,
/// so a link that cannot connect does not hold up PINI.
pub async fn pvalink_link_set_install(db: Arc<PvDatabase>) -> Vec<CommandDef> {
    install(db, crate::server::iocsh::qsrv2_enabled()).await.1
}

async fn install(db: Arc<PvDatabase>, enable: bool) -> (PvaLinkResolver, Vec<CommandDef>) {
    let resolver = install_pvalink_resolver(&db).await;
    let mut commands = register_pvalink_commands(resolver.clone());
    if enable {
        commands.extend(pvalink_enable_commands(resolver.clone()));
    }
    (resolver, commands)
}

/// Declares `pva` on `builder` ahead of its records, as pvxs7x.dbd's
/// `link("pva", "lsetPVX")` does whatever `enable2()` decides, and returns
/// the cell its resolver lands in at link-set install. The decision is taken
/// here, as pvxs takes it at registration.
pub fn register_pva_link(builder: IocBuilder) -> (IocBuilder, Arc<OnceLock<PvaLinkResolver>>) {
    let enable = crate::server::iocsh::qsrv2_enabled();
    let resolver = Arc::new(OnceLock::new());
    let installed = Arc::clone(&resolver);
    let builder = builder.register_link_type("pva", move |db| async move {
        let (resolver, commands) = install(db, enable).await;
        let _ = installed.set(resolver);
        commands
    });
    (builder, resolver)
}

/// [`register_pva_link`] for an [`IocApplication`], whose `.db` loads from
/// its startup script.
pub fn register_pva_link_app(app: IocApplication) -> IocApplication {
    let enable = crate::server::iocsh::qsrv2_enabled();
    app.register_link_type("pva", move |db| async move { install(db, enable).await.1 })
}

#[cfg(test)]
mod seam_guard {
    use source_guard::{Comments, production};

    /// Every timer pvalink arms in production comes from
    /// `epics_base_rs::runtime::task::{sleep, interval, timeout}`, never from
    /// `tokio::time` — the pvalink twin of `epics-pva-rs`'s
    /// `client_scope_timers_go_through_the_runtime_seam`.
    ///
    /// MEASURED on the stage-5 target image: `link.rs`'s monitor re-subscribe
    /// backoff called `tokio::time::sleep` on the callback pool, panicking the
    /// `cbMedium` worker with *"there is no reactor running"*. The whole
    /// pvalink module runs on the target (it is what `realtime-pva-ioc` mounts),
    /// so the scope here is every file, with no host-only exception.
    #[test]
    fn pvalink_scope_timers_go_through_the_runtime_seam() {
        let files: &[(&'static str, &'static str)] = &[
            (include_str!("link.rs"), "impl PvaLink"),
            (include_str!("integration.rs"), "impl PvaLinkResolver"),
            (include_str!("registry.rs"), "impl PvaLinkRegistry"),
            (include_str!("config.rs"), "pub struct PvaLinkConfig"),
        ];
        // Written split so this assertion cannot match its own source text.
        let literal = concat!("tokio", "::time::");
        for &(src, anchor) in files {
            let prod = production(src, Comments::Strip);
            assert!(
                prod.contains(anchor),
                "production slice no longer covers `{anchor}` — the guard would pass vacuously"
            );
            let hits = prod.lines().filter(|l| l.contains(literal)).count();
            assert_eq!(
                hits, 0,
                "pvalink production scope must arm timers through `runtime::task`; \
                 found {hits} bare `{literal}` near `{anchor}` — on RTEMS that panics \
                 the callback worker at runtime"
            );
        }
    }
}
