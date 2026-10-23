//! [`ChannelOpener`], a child of the registry module so it alone reaches
//! [`PvaLinkRegistry::get_or_open`].

use std::sync::Arc;

use epics_base_rs::server::database::PvDatabase;

use super::PvaLinkRegistry;
use crate::pvalink::config::{PvaLinkConfig, PvaLinkParseError};
use crate::pvalink::link::{PvaLink, PvaLinkError, PvaLinkResult};

/// The one place a channel is opened — [`PvaLinkRegistry::get_or_open`] is
/// private to the registry and this child module: pvxs `pvaOpenLink`'s
/// `local` gate (`pvxs/ioc/pvalink_lset.cpp:70-76`) runs before every
/// `registry.get_or_open`, so a `local:true` link this IOC cannot serve
/// never opens or reuses a channel, whichever path asks. Clones share
/// the resolver's registry and database handles.
#[derive(Clone)]
pub(crate) struct ChannelOpener {
    pub(crate) registry: Arc<PvaLinkRegistry>,
    pub(crate) db: Arc<parking_lot::RwLock<Option<PvDatabase>>>,
}

impl ChannelOpener {
    /// Open or reuse the channel `cfg` names, after the `local` gate.
    /// An empty name opens nothing, as `pvaOpenLink` returns before its
    /// channel (`pvxs/ioc/pvalink_lset.cpp:97-98`), so the link reads as
    /// unconnected.
    pub(crate) async fn open(&self, cfg: &PvaLinkConfig) -> PvaLinkResult<Arc<PvaLink>> {
        if cfg.pv_name.is_empty() {
            return Err(PvaLinkParseError::EmptyPv.into());
        }
        self.check_local_admission(cfg)?;
        self.registry.get_or_open(cfg.clone()).await
    }

    /// The `local`-admission gate [`Self::open`] applies; callers that
    /// register a link's options check it first so a refused link leaves
    /// no entry.
    ///
    /// pvxs applies `pvaLinkConfig::local` inside `pvaOpenLink()`
    /// (`ioc/pvalink_lset.cpp:69-75`), the single open function used for
    /// every link direction: if `local` is set and `dbChannelTest()`
    /// cannot find the channel in this IOC, the link set is cleared
    /// (`plink->lset = NULL`) and the link never opens, reads, or writes a
    /// channel. This helper is the single owner of that decision so a
    /// `local=true` link can never open — or reuse a non-local sibling's
    /// already-open — remote channel. A non-`local` config passes
    /// unconditionally.
    ///
    /// "Local" is `dbChannelTest` alone ([`PvDatabase::channel_test`]):
    /// neither a simple PV nor a QSRV group is a database channel, so
    /// softIocPVX refuses a `local` link to either. Synchronous and taking
    /// no record lock: [`LinkSet::open_link`] asks it under a put's hold,
    /// as `pvaOpenLink` asks under `dbScanLock`.
    ///
    /// [`LinkSet::open_link`]: epics_base_rs::server::database::LinkSet::open_link
    pub(crate) fn check_local_admission(&self, cfg: &PvaLinkConfig) -> PvaLinkResult<()> {
        let local = || {
            self.db
                .read()
                .as_ref()
                .is_some_and(|db| db.channel_test(cfg.pv_name.as_bytes()))
        };
        if !cfg.local || local() {
            return Ok(());
        }
        Err(PvaLinkError::NotLocal(cfg.pv_name.clone()))
    }
}
