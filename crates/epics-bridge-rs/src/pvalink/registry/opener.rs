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
/// the resolver's registry, database and QSRV handles.
#[derive(Clone)]
pub(crate) struct ChannelOpener {
    pub(crate) registry: Arc<PvaLinkRegistry>,
    pub(crate) db: Arc<parking_lot::RwLock<Option<PvDatabase>>>,
    #[cfg(feature = "qsrv-core")]
    pub(crate) qsrv: Arc<parking_lot::RwLock<Option<Arc<crate::qsrv::BridgeProvider>>>>,
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
        self.check_local_admission(cfg).await?;
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
    /// "Local" means the target is hosted by this IOC under one of three
    /// forms, all checked without a remote search:
    ///   * a `PvDatabase` record or one of its fields (channel-level, like
    ///     `dbChannelTest()` — see [`Self::is_local_in_db`]),
    ///   * a simple PV registered via `add_pv` (a QSRV single-record
    ///     channel, an iocsh stats PV, a gateway shadow PV),
    ///   * a QSRV group composite PV, which lives only in the QSRV
    ///     provider's group registry, not the `PvDatabase`.
    ///
    /// Gating on `get_record`/the `PvDatabase` alone wrongly rejected
    /// simple and group PVs; only a genuinely remote PV is rejected.
    pub(crate) async fn check_local_admission(&self, cfg: &PvaLinkConfig) -> PvaLinkResult<()> {
        if !cfg.local {
            return Ok(());
        }
        let pv_name = &cfg.pv_name;
        // `mut` is only consumed by the QSRV fallthrough below; gate it so
        // a `qsrv`-less build does not warn unused_mut.
        #[cfg(feature = "qsrv-core")]
        let mut is_local = self.is_local_in_db(pv_name).await;
        #[cfg(not(feature = "qsrv-core"))]
        let is_local = self.is_local_in_db(pv_name).await;
        // QSRV group / single composite PVs: only checked when a QSRV
        // provider is wired. `hosts_pv` covers both the group registry and
        // the provider's single-channel name set.
        #[cfg(feature = "qsrv-core")]
        if !is_local {
            let provider = self.qsrv.read().clone();
            if let Some(provider) = provider {
                is_local = provider.hosts_pv(pv_name.as_bytes()).await;
            }
        }
        if !is_local {
            return Err(PvaLinkError::NotLocal(pv_name.clone()));
        }
        Ok(())
    }

    /// Whether `pv_name` names a channel hosted by the attached
    /// `PvDatabase` — the bridge equivalent of EPICS `dbChannelTest()`.
    ///
    /// Locality is channel-level, not record-name-only: a `record.FIELD`
    /// channel is local when the record exists (alias-aware) and the
    /// field resolves, exactly as `dbChannelTest()` accepts `x`, `x.VAL`,
    /// `x.NAME`, `x.INP` and rejects nonexistent records/fields
    /// (`modules/database/test/ioc/db/dbChannelTest.c:173-181`). A
    /// `{...}` channel-filter suffix and a trailing `$` long-string
    /// modifier are stripped and ignored for the locality decision, and
    /// an empty/trailing-dot field (`x.`, `x.{}`) resolves to the
    /// default value field — matching the modifiers the same test allows
    /// (`dbChannelTest.c:184-186`).
    ///
    /// All lookups are local-only (`find_pv`/`get_record` consult the
    /// simple-PV, record, and alias maps; never a remote search), so the
    /// `local` check never resolves off-IOC. Returns `false` when no
    /// database is attached.
    async fn is_local_in_db(&self, pv_name: &str) -> bool {
        use epics_base_rs::server::database::{filters::split_channel_name, parse_pv_name};

        // Clone the Option out and drop the RwLock guard before any
        // await — holding a parking_lot guard across an await point
        // can stall or deadlock the executor.
        let db = self.db.read().clone();
        let Some(db) = db else {
            return false;
        };

        // Strip any `{...}` channel-filter / JSON suffix first; the
        // remaining `record[.field]` is what decides locality.
        let record_path = split_channel_name(pv_name).record_path;

        // Exact simple `add_pv` PV (filter-stripped name).
        let simple = match std::str::from_utf8(&record_path) {
            Ok(path) => db.find_pv(path).await.is_some(),
            Err(_) => false,
        };
        if simple {
            return true;
        }

        // `record[.field]`: the record must exist (alias-aware) and the
        // field must resolve. `parse_pv_name` maps a bare record to its
        // default `VAL` field; the same default applies after stripping
        // a `$` long-string modifier or an empty/trailing-dot field.
        let (base, field) = parse_pv_name(&record_path);
        let field = field.strip_suffix('$').unwrap_or(field);
        let field = if field.is_empty() { "VAL" } else { field };
        match db.get_record(base) {
            Some(rec) => rec.read().resolve_field(field).is_some(),
            None => false,
        }
    }
}
