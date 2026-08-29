//! `pvalink` — PVA links for EPICS record INP/OUT fields.
//!
//! When a record's INP (or OUT) field carries a link string of the form
//! `pva://<remote-pv>`, this module resolves that link to a live PVA
//! client that periodically reads the remote PV (INP) or pushes record
//! output to it (OUT).
//!
//! `pva://` and not `@pva://`, and the two are not alternatives: a
//! leading `@` is the INST_IO sigil, so `try_parse_hw_link`
//! (`epics-base-rs` `link.rs:1074-1086`) claims the field and returns
//! `ParsedLink::Hw` before the scheme arm is ever consulted. `iocInit`
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
