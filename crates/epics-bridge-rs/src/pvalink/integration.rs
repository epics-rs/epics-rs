//! Wire pvalink up to the record-link resolver in `epics-base-rs`.
//!
//! The integration plan:
//!
//! 1. `PvaLinkResolver` owns a [`PvaLinkRegistry`] (PvaLink cache) and
//!    submits background work through the `epics_base_rs::runtime::task`
//!    spawn seam — `tokio::spawn` on the host, the callback pool on the
//!    RTEMS target where no tokio runtime exists.
//! 2. [`install_pvalink_resolver`] registers the resolver as the database's
//!    `pva` link set. Records with `INP=pva://...` then resolve through the
//!    registry instead of returning `None`.
//! 3. INP links are pre-warmed via [`PvaLinkResolver::open`] (also exposed
//!    as the `pvxr` iocsh command) so the synchronous cached read can
//!    return the monitor value without blocking on a fresh GET.
//!    Out-of-band reads still work — `block_on` will issue a GET — but
//!    pre-warmed monitors are always cheaper.
//!
//! pvxs equivalent: `ioc/pvalink.cpp` + `ioc/pvalink_channel.cpp`
//! (`pvalinkInit`, `pvalinkOpen`, `dbpvxr`).

// RTEMS-EXEC-MODEL-ALLOW(39): checked - these run and pass in the exec-backend
// suite.

use std::sync::Arc;

use epics_base_rs::runtime::task;
use epics_base_rs::server::database::{
    LinkForward, LinkForwardRefused, LinkHolder, LinkOpen, LinkPutOp, LinkSet, PutAdmission,
    PvDatabase,
};
use epics_base_rs::server::record::{
    JlinkValue, PVAJSON_IDENTITY_SEP, pvajson_identity_key, pvajson_identity_parts,
};
use epics_base_rs::types::{DbfLinkClass, EpicsValue, RecordName};
use epics_pva_rs::pvdata::{PvField, ScalarValue};

use super::config::{LinkDirection, PvaLinkConfig, PvaLinkParseError};
use super::link::{PvaLink, PvaLinkError, PvaLinkResult, ScanEvent, ScanOverrun};
use super::registry::{ChannelOpener, PvaLinkRegistry};

/// Resolver wrapping a [`PvaLinkRegistry`]. Cheap to clone — every field is
/// `Arc`-backed. Background work is submitted through the
/// `epics_base_rs::runtime::task` spawn seam, so the resolver holds no tokio
/// runtime handle of its own (the seam picks tokio or the callback pool by
/// target).
#[derive(Clone)]
pub struct PvaLinkResolver {
    /// The executor every link-staging task is spawned through.
    ///
    /// The `LinkSet` entry points below (`forward_handle`, the resolver
    /// closure) are called synchronously from the record-processing thread,
    /// which is not a reactor thread. Holding the capability makes that a
    /// property of construction rather than of whoever happens to call the
    /// link — the reason [`Default`] no longer exists for this type.
    reactor: epics_base_rs::runtime::task::Reactor,
    registry: Arc<PvaLinkRegistry>,
    /// Counter incremented on every successful link read. Used by
    /// `pvalinkrefdiff` to report "links touched since last call". Wraps
    /// at u64::MAX.
    reads: Arc<std::sync::atomic::AtomicU64>,
    /// Master enable flag. Set false via [`Self::set_enabled`] (or
    /// the `pvalink_disable` iocsh command) to make every resolve
    /// return None — useful for site-level pvalink kill switches.
    /// An epics-rs addition: pvxs has no `pvalink_disable`, and its
    /// `pvalink_enable` (`pvxs/ioc/pvalink.cpp:328`) registers `dbpvar`.
    enabled: Arc<std::sync::atomic::AtomicBool>,
    /// Per-PV link-option overrides.
    ///
    /// The `epics-base-rs` link parser collapses `pva://X?sevr=MS`
    /// (and the legacy `pva://X MS` suffix form) down to a bare PV
    /// name in `ParsedLink::Pva`, dropping every query option before
    /// the bridge resolver is consulted. To keep B2 (`MS`/`NMS`) and
    /// the B4 options effective on the resolver hot path, the bridge
    /// stashes the parsed [`PvaLinkConfig`] here keyed by PV name when
    /// a full link string is opened via [`Self::open_link`]; the
    /// resolver then reuses those options instead of the `NMS`/Q=4
    /// defaults. Mirrors the role of pvxs `pvaLinkConfig` carried on
    /// the `jlink` for the lifetime of the link.
    link_options: Arc<LinkCfgMap>,
    /// Per-PV OUT link-option overrides.
    ///
    /// Mirrors `link_options` for the OUT direction: `put_value` uses
    /// these to carry the operator's `proc`, `field`, `defer`, `retry`
    /// settings to the upstream PUT, instead of building a fresh
    /// default config on every write. Populated by [`Self::open_out_link`].
    out_link_options: Arc<LinkCfgMap>,
    /// Database handle used by the B3 scan-on-update forwarder to
    /// process owning records. `None` until [`install_pvalink_resolver`]
    /// wires it — without it, monitor events still update the cached
    /// value but cannot drive `CP`/`CPP` record processing.
    db: Arc<parking_lot::RwLock<Option<PvDatabase>>>,
    /// B3: per-monitor-variant set of record names to process when a
    /// monitor event arrives (the `scan_on_update` / CP fan-out
    /// targets). Populated by [`Self::open_link_for_record`]. Keyed by
    /// [`MonitorKey`] — NOT bare PV name — so two records linking the
    /// same PV with different `Q` / `pipeline` each fan out from their
    /// own monitor.
    scan_targets: Arc<parking_lot::RwLock<std::collections::HashMap<MonitorKey, ScanFanout>>>,
    /// B3: monitor variants whose notification forwarder task is already
    /// running, so [`Self::open_link`] spawns it at most once per
    /// variant. Keyed by [`MonitorKey`] so distinct `Q` / `pipeline`
    /// variants of one PV each get their own forwarder draining their
    /// own monitor receiver.
    forwarders: Arc<parking_lot::Mutex<std::collections::HashSet<MonitorKey>>>,
    /// The link each record field carries — pvxs's `pvaLink` on its
    /// channel's `links`. The one owner of a holder's back-index entry and
    /// CP/CPP scan target: [`Self::attach`] replaces whatever the holder
    /// carried, [`Self::detach`] removes it, so no open can leave a holder
    /// scanned by a channel it no longer links.
    holders: Arc<parking_lot::Mutex<std::collections::HashMap<Holder, Attached>>>,
    /// Source of the id in [`Attachment::Open`].
    next_attach: Arc<std::sync::atomic::AtomicU64>,
    /// B4 `local`: optional handle to the QSRV provider's name
    /// registry. When the IOC also runs QSRV (the common dual-server
    /// deployment), a `local=true` link may target a QSRV group
    /// composite PV — which lives only in the provider's group
    /// registry, not the `PvDatabase`. Without this handle the
    /// locality check sees only records / simple PVs and wrongly
    /// rejects a group-PV link with `NotLocal`. `None` for a
    /// pvalink-only deployment with no QSRV, where group-PV locality
    /// is simply unavailable. Wired via [`Self::with_qsrv_provider`].
    #[cfg(feature = "qsrv-core")]
    qsrv: Arc<parking_lot::RwLock<Option<Arc<crate::qsrv::BridgeProvider>>>>,
    /// Every channel open goes through it.
    opener: ChannelOpener,
}

/// Identity of an INP monitor variant: the subset of the registry key
/// that distinguishes two monitors of the same upstream PV. Mirrors
/// pvxs `channels_key_t = (channelName, pvRequest)` where the pvRequest
/// encodes `pipeline` / `queueSize` (`pvxs/ioc/pvalink.h:116-120`).
///
/// Scan fan-out and forwarder dedup key on this rather than the bare PV
/// name so a `Q=1` record's CP/CPP scans are driven by its own monitor,
/// not whichever variant opened first. Two
/// links that differ only in `field` / `sevr` share one `MonitorKey`:
/// the registry already folds them onto one subscription, so one
/// forwarder correctly drives both (their per-`field` change tracking
/// is independent inside `run_notify_forwarder`).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct MonitorKey {
    pv_name: String,
    pipeline: bool,
    queue_size: usize,
}

impl MonitorKey {
    fn from_config(cfg: &PvaLinkConfig) -> Self {
        Self {
            pv_name: cfg.pv_name.clone(),
            pipeline: cfg.pipeline,
            queue_size: cfg.queue_size,
        }
    }
}

/// A record field carrying a pvalink: `plink->precord` and its field.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct Holder {
    record: RecordName,
    field: Box<str>,
    class: Option<DbfLinkClass>,
}

impl From<LinkHolder<'_>> for Holder {
    fn from(h: LinkHolder<'_>) -> Self {
        Self {
            record: RecordName::from(h.record),
            field: h.field.into(),
            class: h.class(),
        }
    }
}

impl Holder {
    /// pvxs `pvaLink::scanOnUpdate` (`pvxs/ioc/pvalink_link.cpp:122-133`):
    /// only a `DBF_INLINK` with `proc=CP`/`CPP` scans its record; an output
    /// or forward link never does, whatever its `proc`.
    fn scans_on_update(&self, cfg: &PvaLinkConfig) -> bool {
        self.class == Some(DbfLinkClass::InLink) && cfg.scan_on_update
    }
}

/// The link a [`Holder`] carries.
struct Attached {
    cfg: LinkCfg,
    state: Attachment,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Attachment {
    /// On its channel's back-index and fan-out. The id tells this
    /// attachment from a later one of the same holder, for an open that
    /// finishes after the holder moved on.
    Open(u64),
    /// Refused by the `local` gate: pvxs's `plink->lset = NULL`
    /// (`pvxs/ioc/pvalink_lset.cpp:70-75`), registered nowhere until a put
    /// replaces it.
    Refused,
}

/// The direction pvalink gives a link of `class`: a `DBF_OUTLINK` writes,
/// every other link — `FLNK` included, see [`install_pvalink_resolver`] —
/// keeps the INP config.
fn holder_direction(class: Option<DbfLinkClass>) -> LinkDirection {
    match class {
        Some(DbfLinkClass::OutLink) => LinkDirection::Out,
        _ => LinkDirection::Inp,
    }
}

/// Per-monitor-variant scan-on-update fan-out state (B3).
#[derive(Default)]
struct ScanFanout {
    /// Records to process on every monitor event. Each entry mirrors
    /// one INP pvalink whose `proc` is `CP` (scans always) or `CPP`
    /// (scans when the owner is Passive). At our integration
    /// granularity both reduce to "process this record on each event".
    records: Vec<ScanTarget>,
}

/// One record bound to a CP/CPP pvalink (B3).
struct ScanTarget {
    record: RecordName,
    /// The holder field, so [`PvaLinkResolver::detach`] removes this link's
    /// target and no sibling field's.
    field: Box<str>,
    /// pvxs `pvaLinkConfig::monorder` — lower scans first within one
    /// monitor batch.
    monorder: i32,
    /// pvxs `pvaLinkConfig::atomic` — atomic links scan as one
    /// contiguous group ahead of the non-atomic targets in the same
    /// monitor batch, under one multi-record lock so siblings stay
    /// mutually consistent within the batch.
    atomic: bool,
    /// pvxs distinguishes `CP` (scanOnUpdateYes) from `CPP`
    /// (scanOnUpdatePassive). `CPP` is gated by the owning record's
    /// SCAN being Passive (pvxs/ioc/pvalink_channel.cpp:313). True here means
    /// "skip processing when the owning record's SCAN != Passive".
    passive_only: bool,
}

impl PvaLinkResolver {
    pub fn new(reactor: epics_base_rs::runtime::task::Reactor) -> Self {
        let registry = Arc::new(PvaLinkRegistry::new());
        let db = Arc::new(parking_lot::RwLock::new(None));
        #[cfg(feature = "qsrv-core")]
        let qsrv = Arc::new(parking_lot::RwLock::new(None));
        Self {
            reactor,
            opener: ChannelOpener {
                registry: registry.clone(),
                db: db.clone(),
                #[cfg(feature = "qsrv-core")]
                qsrv: qsrv.clone(),
            },
            registry,
            reads: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            enabled: Arc::new(std::sync::atomic::AtomicBool::new(true)),
            link_options: Arc::new(parking_lot::RwLock::new(std::collections::HashMap::new())),
            out_link_options: Arc::new(parking_lot::RwLock::new(std::collections::HashMap::new())),
            db,
            scan_targets: Arc::new(parking_lot::RwLock::new(std::collections::HashMap::new())),
            forwarders: Arc::new(parking_lot::Mutex::new(std::collections::HashSet::new())),
            holders: Arc::new(parking_lot::Mutex::new(std::collections::HashMap::new())),
            next_attach: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            #[cfg(feature = "qsrv-core")]
            qsrv,
        }
    }

    /// Attach the database handle the B3 scan-on-update forwarder
    /// uses to process owning records. Called by
    /// [`install_pvalink_resolver`].
    pub fn attach_database(&self, db: PvDatabase) {
        *self.db.write() = Some(db);
    }

    /// Attach the QSRV provider so a `local=true` link can resolve a
    /// QSRV group composite PV as IOC-local (B4 `local`).
    ///
    /// In a dual-server IOC that runs both pvalink and QSRV, group
    /// composite PVs live only in the provider's group registry — not
    /// the `PvDatabase` — so the bare record / simple-PV locality
    /// check would reject a `local=true` link to a group PV with
    /// `NotLocal`. Wiring this handle lets the check also accept any
    /// name the QSRV provider hosts as a group (or single) channel.
    ///
    /// Optional: a pvalink-only deployment never calls this, and
    /// group-PV locality is then simply unavailable (a `local` link
    /// must target a record or simple PV, as before).
    #[cfg(feature = "qsrv-core")]
    pub fn attach_qsrv_provider(&self, provider: Arc<crate::qsrv::BridgeProvider>) {
        *self.qsrv.write() = Some(provider);
    }

    /// Builder form of [`Self::attach_qsrv_provider`] — wires the
    /// QSRV provider and returns `self` for chaining at IOC assembly.
    #[cfg(feature = "qsrv-core")]
    pub fn with_qsrv_provider(self, provider: Arc<crate::qsrv::BridgeProvider>) -> Self {
        self.attach_qsrv_provider(provider);
        self
    }

    /// Master enable / disable. When disabled, the link set
    /// returns `None` for every lookup so dependent records see
    /// LINK/INVALID alarms but no stale cached values bleed through.
    /// An epics-rs addition; pvxs has no such switch (see `enabled`).
    pub fn set_enabled(&self, on: bool) {
        self.enabled.store(on, std::sync::atomic::Ordering::Relaxed);
    }

    /// Read the current enable flag.
    pub fn is_enabled(&self) -> bool {
        self.enabled.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Open / cache a link for `pv_name` in INP+monitor mode. Mirrors
    /// pvxs `pvalinkOpen` (pvxs/ioc/pvalink_channel.cpp). After this returns,
    /// later calls to [`LinkSet::get_cached_value`] for the same name will read
    /// the cached monitor value (no async block).
    ///
    /// Honors any link options previously registered for `pv_name`
    /// via [`Self::open_link`] — otherwise pvxs defaults apply.
    pub async fn open(&self, pv_name: &str) -> PvaLinkResult<Arc<PvaLink>> {
        let cfg = self.inp_cfg_for(pv_name);
        self.opener.open(&cfg).await
    }

    /// Open / cache a link from a full `pva://...` link string,
    /// parsing and retaining its options (`sevr`, `Q`, `pipeline`,
    /// `monorder`, ...). The parsed [`PvaLinkConfig`] is stashed under
    /// the bare PV name so the steady-state resolver hot path —
    /// driven by `epics-base-rs`, which only ever hands the bridge a
    /// bare PV name — keeps applying the same options.
    ///
    /// This is the entry point that makes B2 (`MS`/`NMS`) effective
    /// through the resolver: an INP record whose link string carries
    /// `sevr=MS` is registered here at IOC init (or via `pvxr`), and
    /// every later bare-name resolve reuses the `MS` mode.
    pub async fn open_link(&self, link_string: &str) -> PvaLinkResult<Arc<PvaLink>> {
        self.open_link_inner(link_string, None).await
    }

    /// Like [`Self::open_link`] but also binds `record` as a
    /// scan-on-update target (B3). When the parsed link has
    /// `scan_on_update` (i.e. `proc=CP` / `CPP`), every monitor event
    /// on the remote PV processes `record` through the database —
    /// the INP-monitor record-notification path that pvxs
    /// `pvaLinkChannel::run` drives via `scanOnUpdate`.
    pub async fn open_link_for_record(
        &self,
        link_string: &str,
        holder: LinkHolder<'_>,
    ) -> PvaLinkResult<Arc<PvaLink>> {
        self.open_link_inner(link_string, Some(holder.into())).await
    }

    async fn open_link_inner(
        &self,
        link_string: &str,
        holder: Option<Holder>,
    ) -> PvaLinkResult<Arc<PvaLink>> {
        let cfg = PvaLinkConfig::parse(link_string, LinkDirection::Inp)?;
        // Convenience-URI path: key the per-link options by the full
        // scheme-stripped link string (including any `?query`), so two
        // links to the same PV with different options each keep their
        // own entry.
        let options_key = strip_scheme(link_string.as_bytes())
            .unwrap_or(link_string)
            .to_string();
        self.open_inp_cfg(cfg, options_key, holder).await
    }

    /// Like [`Self::open_link_for_record`] but for a link parsed from
    /// the structured pvxs-parity JSON longhand `{pva:{pv,…}}`
    /// (`epics_base_rs` `PvaJsonLink`). The options arrive as JLink
    /// members, not a `?key=value` query, and
    /// the per-link config is keyed by the per-link IDENTITY KEY
    /// (`pvajson_identity_key`) — epics-base-rs resolves a `PvaJson` link
    /// through `external_pv_name()` → `link_identity_key()`
    /// (`server/database/links.rs`), so the steady-state lset hot path
    /// looks the config up by that same key. Keying by the bare PV would
    /// collapse two same-PV structured links onto one cache slot
    /// (last-writer-wins), losing each link's `field`/`Q`/`pipeline`
    /// (pvxs per-link `pvaLinkConfig`, ioc/pvalink.h:65). `record` is
    /// bound as a CP/CPP scan-on-update target exactly as the
    /// convenience-URI path does.
    pub async fn open_json_link_for_record(
        &self,
        pv: &str,
        options: &[(String, JlinkValue)],
        holder: LinkHolder<'_>,
    ) -> PvaLinkResult<Arc<PvaLink>> {
        let cfg = PvaLinkConfig::from_jlink_options(pv, options, LinkDirection::Inp)?;
        let options_key = pvajson_identity_key(pv, options);
        self.open_inp_cfg(cfg, options_key, Some(holder.into()))
            .await
    }

    /// Register an INP link's options under `options_key`, bind an
    /// optional scan-on-update `record`, open/cache the link, and spawn
    /// its monitor-notification forwarder. The single owner shared by
    /// the convenience-URI path ([`Self::open_link_inner`], keyed by the
    /// full link string) and the pvxs-parity JSON path
    /// ([`Self::open_json_link_for_record`], keyed by the per-link
    /// identity key).
    async fn open_inp_cfg(
        &self,
        cfg: PvaLinkConfig,
        options_key: String,
        holder: Option<Holder>,
    ) -> PvaLinkResult<Arc<PvaLink>> {
        // An INP link that is meaningful for the resolver must keep a
        // monitor open; force it on (pvxs treats `proc=CP/CPP` and the
        // resolver path as monitored).
        let cfg = entry(cfg, LinkDirection::Inp);
        let attached = holder
            .as_ref()
            .and_then(|h| self.attach(h, &cfg).map(|id| (h, id)));
        // B4 `local`: a `local`-flagged link must resolve a PV served by
        // *this* IOC. Reject up-front so the operator gets a clear error
        // instead of a silent remote resolution. The identical gate runs
        // on the OUT open and lazy-write paths — see
        // [`ChannelOpener::check_local_admission`].
        self.admit(holder.as_ref(), &cfg).await?;
        // Register the per-link options under the caller-chosen key:
        // the full scheme-stripped link string for the convenience-URI
        // path (two links to the same PV with different options each keep
        // their own entry), or the per-link identity key
        // (`pvajson_identity_key`) for the pvxs-parity JSON path (the same
        // key epics-base-rs hands the lset at resolve time via
        // `external_pv_name()` → `link_identity_key()`). Both forms keep
        // two same-PV links with differing options on distinct entries.
        // pvxs equivalent: each `pvaLink` carries its own `pvaLinkConfig`
        // (`pvxs/ioc/pvalink.h:65`). The registry key shares the channel
        // by (pv_name, pipeline, queue_size) per `pvxs/ioc/pvalink.h:116`.
        self.link_options.write().insert(options_key, cfg.clone());
        let link = self.opener.open(&cfg).await?;
        if let Some((holder, id)) = attached {
            self.finish_inp_attach(holder, &cfg, id, &link).await;
        } else {
            self.spawn_notify_forwarder(MonitorKey::from_config(&cfg), &link);
        }
        Ok(link)
    }

    /// pvxs `pvaOpenLink`'s attach (`pvxs/ioc/pvalink_lset.cpp:139-171`)
    /// for `holder`: whatever link it carried is [`Self::detach`]ed, then
    /// `cfg`'s channel gets the record on its back-index and, for an INP
    /// `proc=CP`/`CPP` link, as a scan target. `None` when the holder
    /// already carries this very link, open or refused — the iocInit open
    /// of a link the install scan attached — so nothing is attached or
    /// reported twice.
    fn attach(&self, holder: &Holder, cfg: &LinkCfg) -> Option<u64> {
        let mut holders = self.holders.lock();
        if holders.get(holder).is_some_and(|a| *a.cfg == **cfg) {
            return None;
        }
        if let Some(old) = holders.remove(holder) {
            self.unregister(holder, &old);
        }
        self.registry.attach_record(cfg, holder.record.as_bytes());
        // B3: keyed by the monitor variant identity, NOT bare PV name, so
        // a `Q=1` record's scan targets are not merged with a `Q=64`
        // sibling's (pvxs keys links onto their channel by
        // `(channelName, pvRequest)`, `pvxs/ioc/pvalink.h:116-120`).
        if cfg.direction == LinkDirection::Inp && holder.scans_on_update(cfg) {
            self.scan_targets
                .write()
                .entry(MonitorKey::from_config(cfg))
                .or_default()
                .records
                .push(ScanTarget {
                    record: holder.record.clone(),
                    field: holder.field.clone(),
                    monorder: cfg.monorder,
                    atomic: cfg.atomic,
                    passive_only: cfg.scan_on_passive,
                });
        }
        let id = self
            .next_attach
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        holders.insert(
            holder.clone(),
            Attached {
                cfg: cfg.clone(),
                state: Attachment::Open(id),
            },
        );
        Some(id)
    }

    /// The `local` gate ([`ChannelOpener::check_local_admission`]); a link
    /// it refuses stays on `holder` as [`Attachment::Refused`].
    async fn admit(&self, holder: Option<&Holder>, cfg: &PvaLinkConfig) -> PvaLinkResult<()> {
        let admitted = self.opener.check_local_admission(cfg).await;
        if admitted.is_err()
            && let Some(holder) = holder
        {
            self.refuse(holder, cfg);
        }
        admitted
    }

    /// `holder`'s link `cfg` has no lset from here on.
    fn refuse(&self, holder: &Holder, cfg: &PvaLinkConfig) {
        let mut holders = self.holders.lock();
        if let Some(attached) = holders.get_mut(holder).filter(|a| *a.cfg == *cfg) {
            self.unregister(holder, attached);
            attached.state = Attachment::Refused;
        }
    }

    /// pvxs `pvaRemoveLink` → `~pvaLink` (`pvxs/ioc/pvalink_link.cpp:27-35`):
    /// `holder` leaves its channel's `links`, so that channel's monitor no
    /// longer scans it. `cfg` names the link being removed; a holder that
    /// has since moved to another link keeps it.
    fn detach(&self, holder: &Holder, cfg: &PvaLinkConfig) {
        let mut holders = self.holders.lock();
        if holders.get(holder).is_some_and(|a| *a.cfg == *cfg)
            && let Some(old) = holders.remove(holder)
        {
            self.unregister(holder, &old);
        }
    }

    /// Undo what [`Self::attach`] registered for `holder`'s link, if it is
    /// open. Called under the `holders` lock.
    fn unregister(&self, holder: &Holder, attached: &Attached) {
        if attached.state == Attachment::Refused {
            return;
        }
        let cfg = &*attached.cfg;
        self.registry.detach_record(cfg, holder.record.as_bytes());
        if let Some(fanout) = self
            .scan_targets
            .write()
            .get_mut(&MonitorKey::from_config(cfg))
        {
            fanout
                .records
                .retain(|t| !(t.record == holder.record && t.field == holder.field));
        }
    }

    /// Whether `holder` still carries attachment `id`.
    fn is_attached(&self, holder: &Holder, id: u64) -> bool {
        self.holders
            .lock()
            .get(holder)
            .is_some_and(|a| a.state == Attachment::Open(id))
    }

    /// The rest of an INP attach once its channel is open: the variant's
    /// forwarder, then the attach-time scan
    /// (`pvxs/ioc/pvalink_lset.cpp:148-167`, `scanOnce(plink->precord)`):
    /// when the shared channel is ALREADY connected at attach, fire one
    /// immediate scan of just this holder. A freshly-opened monitor
    /// variant is not yet connected here and gets its first scan from the
    /// monitor task's `on_event`.
    async fn finish_inp_attach(
        &self,
        holder: &Holder,
        cfg: &PvaLinkConfig,
        id: u64,
        link: &Arc<PvaLink>,
    ) {
        self.spawn_notify_forwarder(MonitorKey::from_config(cfg), link);
        if holder.scans_on_update(cfg) && link.is_connected() && self.is_attached(holder, id) {
            self.scan_attached_record(&holder.record, cfg.scan_on_passive);
        }
    }

    /// The open [`LinkSet::open_link`] stages for attachment `id`, on the
    /// reactor: the `local` gate, the channel, and for an INP link the
    /// rest of [`Self::finish_inp_attach`]. Reports nothing: the install
    /// scan is what prints a refused link's line.
    async fn open_attached(&self, holder: Holder, cfg: LinkCfg, id: u64) {
        if self.admit(Some(&holder), &cfg).await.is_err() {
            return;
        }
        let Ok(link) = self.opener.open(&cfg).await else {
            return;
        };
        if cfg.direction == LinkDirection::Inp {
            self.finish_inp_attach(&holder, &cfg, id, &link).await;
        }
    }

    /// B3: spawn the monitor-notification forwarder, at most once per
    /// monitor variant. The task drains that variant's notify receiver
    /// and, for every event, processes the records registered as
    /// scan-on-update targets for the same variant (`monorder`-sorted;
    /// `always` links also process on no-op updates).
    fn spawn_notify_forwarder(&self, key: MonitorKey, link: &Arc<PvaLink>) {
        {
            let mut started = self.forwarders.lock();
            if started.contains(&key) {
                return;
            }
            started.insert(key.clone());
        }
        let Some(rx) = link.take_notify_rx() else {
            // OUT / non-monitor links never created a channel.
            self.forwarders.lock().remove(&key);
            return;
        };
        let scan_targets = self.scan_targets.clone();
        let db = self.db.clone();
        // The link's coalescing overrun accounting: the monitor task arms
        // it on a full queue, the forwarder drains the owed scan, so no
        // CP/CPP scan is silently lost under a saturated queue.
        let scan_overrun = link.scan_overrun();
        // field is now per-ScanTarget (not shared across all
        // targets). `run_notify_forwarder` reads each target's own field.
        self.reactor.spawn(run_notify_forwarder(
            key,
            rx,
            scan_targets,
            db,
            scan_overrun,
        ));
    }

    /// The attach-time scan pvxs runs when the shared channel is already
    /// connected at link-open (`pvxs/ioc/pvalink_lset.cpp:148-167`): the
    /// CPP passive gate, then C `scanOnce`. Unlike the steady-state
    /// [`scan_target_already_locked`] there is no PACT pre-check; the
    /// queued `dbProcess` meets a busy record as C's does.
    fn scan_attached_record(&self, record: &[u8], passive_only: bool) {
        let Some(db_handle) = self.db.read().clone() else {
            return;
        };
        let passive = db_handle.get_record(record).is_some_and(|rec| {
            rec.read().common.scan == epics_base_rs::server::record::ScanType::Passive
        });
        if passive || !passive_only {
            db_handle.scan_once(record);
        }
    }

    /// The INP config of the link `full` names (see [`link_cfg`]). `full`
    /// may be a bare PV name, a query-bearing string (`PV?field=F&proc=CPP`)
    /// or a structured identity key.
    fn inp_cfg_for(&self, full: &str) -> LinkCfg {
        link_cfg(&self.link_options, full, LinkDirection::Inp)
    }

    /// The OUT config of the link `full` names (see [`link_cfg`]).
    fn out_cfg_for(&self, full: &str) -> LinkCfg {
        link_cfg(&self.out_link_options, full, LinkDirection::Out)
    }

    /// Open / cache an OUT link from a full `pva://...` link string,
    /// parsing and retaining its options (`proc`, `field`, `defer`,
    /// `retry`, ...). The parsed [`PvaLinkConfig`] is stashed under the
    /// bare PV name so the `put_value` resolver hot-path picks up the
    /// operator's proc / field / defer settings on every write.
    ///
    /// Mirrors [`Self::open_link`] for the OUT direction. pvxs equivalent:
    /// `pvaLinkConfig` carried on the `jlink` (pvxs/ioc/pvalink_jlif.cpp).
    pub async fn open_out_link(
        &self,
        link_string: &str,
        holder: Option<LinkHolder<'_>>,
    ) -> PvaLinkResult<Arc<PvaLink>> {
        let cfg = PvaLinkConfig::parse(link_string, LinkDirection::Out)?;
        // key by full link string (same rationale as open_link_inner).
        let options_key = strip_scheme(link_string.as_bytes())
            .unwrap_or(link_string)
            .to_string();
        self.open_out_cfg(cfg, options_key, holder).await
    }

    /// OUT counterpart of [`Self::open_json_link_for_record`]: open an
    /// OUT link from the structured pvxs-parity JSON longhand
    /// (`epics_base_rs` `PvaJsonLink`), reading options as JLink members
    /// rather than a `?key=value` query. Keyed
    /// by the per-link IDENTITY KEY (`pvajson_identity_key`) — epics-base-rs
    /// writes a `PvaJson` OUT link through `external_pv_name()` →
    /// `link_identity_key()` (`server/database/links.rs`), so `put_value`
    /// looks the config up by that same key (two same-PV structured OUT
    /// links keep distinct `field`/`proc`/`defer` configs).
    pub async fn open_json_out_link(
        &self,
        pv: &str,
        options: &[(String, JlinkValue)],
        holder: Option<LinkHolder<'_>>,
    ) -> PvaLinkResult<Arc<PvaLink>> {
        let cfg = PvaLinkConfig::from_jlink_options(pv, options, LinkDirection::Out)?;
        let options_key = pvajson_identity_key(pv, options);
        self.open_out_cfg(cfg, options_key, holder).await
    }

    /// Register an OUT link's options under `options_key` and open/cache
    /// the link. Shared by the convenience-URI path
    /// ([`Self::open_out_link`], keyed by the full link string) and the
    /// pvxs-parity JSON path ([`Self::open_json_out_link`], keyed by the
    /// per-link identity key).
    async fn open_out_cfg(
        &self,
        cfg: PvaLinkConfig,
        options_key: String,
        holder: Option<LinkHolder<'_>>,
    ) -> PvaLinkResult<Arc<PvaLink>> {
        // `local`-admission gate before any registration or open, so a
        // rejected `local=true` OUT link leaves no stale option entry and
        // never opens — or reuses a sibling's already-open — remote
        // channel (pvxs `pvaOpenLink`, direction-agnostic).
        let cfg = entry(cfg, LinkDirection::Out);
        let holder = holder.map(Holder::from);
        // Back-index the owning record for `dbpvar`'s record-name glob
        // filter (pvxs `pvaLinkChannel::links`).
        if let Some(holder) = &holder {
            self.attach(holder, &cfg);
        }
        self.admit(holder.as_ref(), &cfg).await?;
        self.out_link_options
            .write()
            .insert(options_key, cfg.clone());
        self.opener.open(&cfg).await
    }

    /// Number of successful link reads since startup.
    pub fn read_count(&self) -> u64 {
        self.reads.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Number of cached links.
    pub fn link_count(&self) -> usize {
        self.registry.len()
    }

    /// STAGE-5 PROBE: a snapshot of the one IOC-wide client, so a target
    /// console can report how many upstream TCP connections back N links.
    pub fn client_report(&self) -> epics_pva_rs::client_native::context::ClientReport {
        self.registry.client().report_zeroed(false)
    }

    /// Per-channel pvalink diagnostics, backing the `dbpvar` IOC shell
    /// command (pvxs `dbpvxr`, `pvxs/ioc/pvalink.cpp:184-316`).
    pub fn channel_diagnostics(&self) -> Vec<super::registry::ChannelDiag> {
        self.registry.channel_diagnostics()
    }

    /// Maximize-severity result for the link named `pv_name` (B2).
    ///
    /// Returns the remote EPICS severity that should fold into the
    /// owning record's `LINK_ALARM` — `Some(sev)` only when the
    /// link's `MS`/`MSI` mode says the remote severity propagates;
    /// `None` for `NMS` links, sub-threshold severities, links not
    /// yet open, or links with no cached value. Mirrors pvxs
    /// `pvaGetAlarmMsg`'s severity output (pvxs/ioc/pvalink_lset.cpp:550-552).
    pub fn link_alarm_severity(&self, pv_name: &str) -> Option<i32> {
        // apply the caller's own parsed `sevr` mode and resolve the
        // caller's own monitor variant. Pre-fix this called
        // `link_alarm_severity()` on whichever cached INP link
        // `try_get_any` returned first for the bare PV — that link's
        // `config.sevr` and monitor variant belong to an arbitrary other
        // caller. pvxs `pvaLinkConfig` is per-link and the channel is
        // keyed by `(channelName, pvRequest)`
        // (`pvxs/ioc/pvalink.h:65,116-120`).
        let full = strip_scheme(pv_name.as_bytes())?;
        let cfg = self.inp_cfg_for(full);
        let sevr = cfg.sevr;
        self.registry
            .try_get_inp(&cfg.pv_name, cfg.pipeline, cfg.queue_size)?
            .link_alarm_severity_with(&cfg.field, sevr)
    }

    /// Wait until the link for `pv_name` has received at least one
    /// monitor event (i.e., the cached value is populated). Returns
    /// `false` on timeout. Mirrors pvxs
    /// `testqsrvWaitForLinkConnected` (pvxs/ioc/pvalink.cpp:131) — the
    /// canonical test helper for "wait for the upstream IOC to come
    /// online before continuing".
    pub async fn wait_for_link_connected(
        &self,
        pv_name: &str,
        timeout: std::time::Duration,
    ) -> bool {
        let link = match self.open(pv_name).await {
            Ok(l) => l,
            Err(_) => return false,
        };
        // Poll the link's read() — succeeds once the monitor has
        // delivered at least one event.
        let deadline = std::time::Instant::now() + timeout;
        loop {
            if link.read().await.is_ok() {
                return true;
            }
            if std::time::Instant::now() >= deadline {
                return false;
            }
            task::sleep(std::time::Duration::from_millis(50)).await;
        }
    }
}

/// Report a link that the install scan could not open, on the one channel
/// an IOC operator actually sees.
///
/// The scan used to discard every open result with `let _ =`, so a link
/// rejected at init (bad options, `local=true` naming a PV this IOC does not
/// hold) left NO trace: the record simply never linked, and the only visible
/// difference was a link count one lower than the database implies. That cost
/// a full cross-build / image-copy / qemu-boot cycle to notice on the RTEMS
/// target, where the pre-open of `RTEMS:PVA:UPLNK` was believed to be failing
/// (it was in fact never attempted — `44c4ef3e`).
///
/// `eprintln!` rather than `tracing::warn!` deliberately, on two grounds:
/// pvxs reports the same class of init-time link refusal with
/// `fprintf(stderr, "%s Error: local:true link to '%s' can't be fulfilled\n")`
/// (`ioc/pvalink_lset.cpp:71-72`), and the sibling QSRV group loader in this
/// crate already reports config-load failures the same way
/// (`qsrv/group_config.rs`, `qsrv/provider.rs:1071`). It is also the only
/// path that reaches an RTEMS console: the target installs no `tracing`
/// subscriber, so a `warn!` here would be discarded exactly like the
/// diagnostic it replaces.
fn report_open_failure(
    record: &[u8],
    field: &str,
    channel: &str,
    result: PvaLinkResult<Arc<PvaLink>>,
) {
    let record = String::from_utf8_lossy(record);
    match result {
        Ok(_) => {}
        // pvxs's own line, verbatim (`ioc/pvalink_lset.cpp:71-72`).
        Err(PvaLinkError::NotLocal(pv)) => {
            eprintln!("{record} Error: local:true link to '{pv}' can't be fulfilled");
        }
        // `pvaOpenLink`'s empty-name early-out prints nothing (`:97-98`).
        Err(PvaLinkError::Config(PvaLinkParseError::EmptyPv)) => {}
        Err(e) => eprintln!("{record}.{field} Error: pvalink to '{channel}' not opened: {e}"),
    }
}

/// Install a [`PvaLinkResolver`] on `db`. Returns the resolver so the
/// caller can pre-open links and query stats (`db_pvxr` /
/// `pvalinkrefdiff` iocsh commands lean on this).
///
/// Registers the resolver as the `"pva"` lset, the only route a pva link
/// is read through.
///
/// also pre-registers pvalink options and CP/CPP scan targets
/// for every loaded DB record that carries a JSON-object pvalink with
/// options. pvxs equivalent: options live on the `jlink` struct for
/// the lifetime of the link (pvxs/ioc/pvalink_jlif.cpp).
pub async fn install_pvalink_resolver(db: &Arc<PvDatabase>) -> PvaLinkResolver {
    let resolver = PvaLinkResolver::new(
        epics_base_rs::runtime::task::Reactor::current()
            .expect("install_pvalink_resolver is awaited on the IOC's reactor"),
    );
    // B3: give the resolver the DB handle so the scan-on-update
    // forwarder can process owning records.
    resolver.attach_database((**db).clone());
    // pvxs `linkGlobal_t::alloc` builds the link client's Context here, at
    // `initHookAfterCaLinkInit` (`ioc/iochooks.cpp:338-341`).
    let _ = resolver.registry.client().start().await;
    db.register_link_set("pva", Arc::new(resolver.clone()))
        .await;

    // scan all loaded records and pre-register any pvalink options —
    // carried either as a `ParsedLink::Pva` convenience-URI/legacy-suffix
    // string or as structured `ParsedLink::PvaJson` JLink members. This
    // ensures CP/CPP scan targets are wired before the first monitor
    // event and field/sevr/Q settings are effective from the first
    // read/write without iocsh pre-warming.
    use epics_base_rs::server::record::ParsedLink;
    // `record_link_fields` also yields `FLNK`, and this scan must see it:
    // pvxs installs a pvaLink on a forward link exactly as on an input or
    // output one. `dbInitLink` hands a `JSON_LINK` to `dbJLinkInit`
    // (`dbLink.c:107-110`) *before* any `dbfType` discrimination, `pva_lset`
    // supplies the `DBF_FWDLINK` entry point `pvaScanForward`
    // (`pvxs/ioc/pvalink_lset.cpp:672-688`), and pvxs' own test helper accepts
    // `DBF_FWDLINK` as a pvalink field (`pvxs/ioc/pvalink.cpp:117-127`).
    //
    // An `FLNK` takes the INP arm below on purpose even though a pvxs
    // forward link ends in a Put: [`PvaLinkResolver::forward_handle`] opens
    // the target through `inp_cfg_for` / `get_or_open`, i.e. the INP config and
    // registry entry this arm creates (pvxs' `pvaLinkChannel` always monitors).
    // Routing `FLNK` to `open_out_link` would register its options under the
    // OUT map, where the forward dispatch would never find them.
    for record_name in db.all_record_names().await {
        let Some(record_type) = db
            .get_record(&record_name)
            .map(|rec| rec.read().record.record_type())
        else {
            continue;
        };
        for (field_name, _raw, parsed) in db.record_link_fields(&record_name) {
            let holder = LinkHolder::new(record_type, record_name.as_bytes(), &field_name);
            match &parsed {
                // Convenience-URI / legacy-suffix form: the verbatim
                // channel-name string may carry options in the
                // `?key=value` query form OR the legacy whitespace suffix
                // form (`TARGET MS`, `TARGET CPP`).
                //
                // A BARE PV (no query, no modifiers) is opened here too.
                // `pvaOpenLink` (`pvxs/ioc/pvalink_lset.cpp:63-137`) creates and
                // opens a `pvaLinkChannel` for EVERY pvalink at link-init —
                // both directions, options or not; the one early-out is an
                // empty channel name (`:96-97`). Skipping bare links because
                // "there are no options to pre-register" conflated *carrying
                // options* with *needing a channel*, and the missing channel
                // is load-bearing on the OUT side: `PvaLink::open` starts the
                // OUT connection-tracking monitor, and the non-retry write
                // gate refuses a put while `is_connected()` is false
                // (`valid() == connected && root`, `pvxs/ioc/pvalink_link.cpp:69`).
                // Opening lazily on the first put therefore DROPPED that put.
                // Measured on the RTEMS target, stage 5:
                // `record(ao,"RTEMS:PVA:UPLNK"){field(OUT,"{pva:{pv:'UPSTREAM:AO'}}")}`
                // — a pv-only longhand parses to a plain `Pva`, so it took
                // this arm — never appeared in `channel_diagnostics()` and the
                // banner reported 1 link for 3 link-bearing records.
                ParsedLink::Pva(s) => {
                    let link_str = format!("pva://{s}");
                    let opened = match holder_direction(holder.class()) {
                        LinkDirection::Out => resolver.open_out_link(&link_str, Some(holder)).await,
                        LinkDirection::Inp => {
                            resolver.open_link_for_record(&link_str, holder).await
                        }
                    };
                    report_open_failure(record_name.as_bytes(), &field_name, &link_str, opened);
                }
                // pvxs-parity JSON longhand `{pva:{pv,…}}`: the options
                // are structured JLink members, read directly without a
                // `?key=value` query round-trip.
                // Always pre-registered — the longhand variant exists
                // precisely to carry options (a pv-only longhand yields a
                // plain `Pva`), so there is no bare-PV early-out here.
                ParsedLink::PvaJson(j) => {
                    let opened = match holder_direction(holder.class()) {
                        LinkDirection::Out => {
                            resolver
                                .open_json_out_link(&j.pv, &j.options, Some(holder))
                                .await
                        }
                        LinkDirection::Inp => {
                            resolver
                                .open_json_link_for_record(&j.pv, &j.options, holder)
                                .await
                        }
                    };
                    report_open_failure(record_name.as_bytes(), &field_name, &j.pv, opened);
                }
                _ => {}
            }
        }
    }

    resolver
}

type ScanTargetMap = Arc<parking_lot::RwLock<std::collections::HashMap<MonitorKey, ScanFanout>>>;

/// B3 monitor-notification forwarder loop.
///
/// Drains `rx` (fed by the link's monitor task) and, for every event,
/// processes the records registered as scan-on-update targets for this
/// monitor variant.
///
/// pvxs scans every CP target and every eligible CPP target on EVERY
/// monitor event — and on disconnect — with no value-difference
/// comparison: the `always` option is parsed but ignored at scan time
/// (`pvxs/documentation/pvalink.rst:102`;
/// `pvxs/ioc/pvalink_channel.cpp:389-431` rebuilds and scans the
/// atomic/non-atomic target lists unconditionally after both the
/// value-update branch and the `catch(client::Disconnect&)` branch,
/// `:335-373`). A `Value` and a `Disconnected` event therefore drive
/// the identical scan loop — there is no no-op suppression and no
/// per-field change tracking.
///
/// Ordering (B4): `atomic` targets scan first as one contiguous group
/// under a single multi-record lock, then the non-atomic targets;
/// within each group `monorder` (low → high) decides the order. A `CPP`
/// (`passive_only`) target is skipped only when its owning record's
/// SCAN is not Passive. The loop ends when every sender is dropped
/// (i.e. the link is closed).
async fn run_notify_forwarder(
    monitor_key: MonitorKey,
    mut rx: tokio::sync::mpsc::Receiver<ScanEvent>,
    scan_targets: ScanTargetMap,
    db: Arc<parking_lot::RwLock<Option<PvDatabase>>>,
    scan_overrun: Arc<ScanOverrun>,
) {
    // Every event — `Value` or `Disconnected` — drives the same scan
    // pass. The event payload is intentionally unused: pvxs does not
    // value-diff before scanning (see the doc comment above), and the
    // cached value itself is updated by the monitor task, not here.
    //
    // After each pass, drain any coalesced overruns: when the bounded
    // queue filled, the monitor task armed `scan_overrun` instead of
    // dropping the event (EPICS `db_queue_event_log`,
    // `dbEvent.c:812-827`) — each armed flag owes one more scan with the
    // latest cached value, so a saturated queue never silently skips a
    // CP/CPP process. The queue was full when the flag was set, so the
    // backlog `recv` already woke this task (no lost wakeup), matching
    // EPICS skipping the re-signal because "the event task has already
    // been notified" (`dbEvent.c:823`). A channel-close (`recv` => None)
    // still drains a final owed scan before exiting.
    loop {
        let event = rx.recv().await;
        if event.is_some() {
            scan_once(&monitor_key, &scan_targets, &db).await;
        }
        while scan_overrun.take_pending() {
            scan_once(&monitor_key, &scan_targets, &db).await;
        }
        if event.is_none() {
            break;
        }
    }
}

/// Run one CP/CPP scan pass for `monitor_key`'s registered targets.
/// Atomic targets scan first as one contiguous group under a single
/// multi-record lock, then non-atomic targets; `monorder` orders within
/// each group. The scan trigger carries no value — each target reads the
/// link's `latest` cache through its own INP read (see
/// [`run_notify_forwarder`]).
/// CPP passive gate + PACT pre-check for one scan target, evaluated
/// under the target's write lock — pvxs `ScanTrack::scan`
/// (`pvxs/ioc/pvalink_channel.cpp:313-323`). A target is gated out when:
///   - a `CPP` (`passive_only`) target whose SCAN is not Passive is
///     skipped (pvxs `check_passive && prec->scan != 0`); a non-zero
///     SCAN (Event, I/O Intr, periodic) means the record has its own
///     scan source and must not be re-fired from CPP.
///   - an already-processing (PACT) target gets `rpro = true` and is
///     NOT processed; the standard RPRO mechanism reprocesses it once
///     the async cycle completes (pvxs `else if (prec->pact) {
///     prec->rpro = TRUE; }`). pvxs intercepts PACT *before*
///     `dbProcess` because `dbProcess` on PACT only counts toward
///     SCAN_ALARM and never sets RPRO; without this a fast monitor
///     stream onto a stuck async target drives it to SCAN_ALARM/INVALID
///     after MAX_LOCK consecutive busy attempts.
///
/// Otherwise the record is processed, `dbProcess` without PUTF, and
/// processed WITH links so the scan fans out via INP/OUT/FLNK.
///
/// The caller MUST hold `record`'s lock, as pvxs's does ("caller has locked
/// record", pvalink_channel.cpp:312): the PACT test and the process it gates
/// see one state, so a record that goes active in between cannot fall into
/// `dbProcess`'s own PACT guard. This is the single owner of the scan-time
/// `rpro` set, used by both phases of [`scan_once`].
fn scan_target_already_locked(db_handle: &PvDatabase, record: &[u8], passive_only: bool) {
    let Some(rec) = db_handle.get_record(record) else {
        return;
    };
    {
        let mut tg = rec.write();
        if passive_only && tg.common.scan != epics_base_rs::server::record::ScanType::Passive {
            return;
        }
        if tg.is_processing() {
            tg.common.rpro = 1;
            return;
        }
    }
    let mut visited = epics_base_rs::server::database::ProcStack::new();
    let _ = db_handle.process_record_with_links_already_locked(record, &mut visited);
}

async fn scan_once(
    monitor_key: &MonitorKey,
    scan_targets: &ScanTargetMap,
    db: &Arc<parking_lot::RwLock<Option<PvDatabase>>>,
) {
    // Snapshot the fan-out, then order it: atomic group first,
    // then non-atomic; `monorder` within each group.
    let mut targets: Vec<(RecordName, i32, bool, bool)> = match scan_targets.read().get(monitor_key)
    {
        Some(fanout) => fanout
            .records
            .iter()
            .map(|t| (t.record.clone(), t.monorder, t.atomic, t.passive_only))
            .collect(),
        None => Vec::new(),
    };
    // Sort key: (!atomic, monorder) → atomic (false sorts first),
    // then ascending monorder.
    targets.sort_by_key(|(_, order, atomic, _)| (!*atomic, *order));

    let Some(db_handle) = db.read().clone() else {
        // No database attached yet — skip this scan pass.
        return;
    };

    // pvxs builds a `DBManyLock` over every atomic
    // scan-on-update target record and holds a `DBManyLocker`
    // across the whole atomic scan (`pvxs/ioc/pvalink_channel.cpp:409`
    // and `:422-427`). Acquire the database-level multi-record epoch
    // lock over the atomic target set *before* scanning any of
    // them, and hold it across the whole atomic group, so a
    // direct writer, another scan, or an atomic sibling cannot
    // interleave between the atomic target records. Non-atomic
    // targets are scanned individually afterwards — matching
    // pvxs, which gives each non-atomic record its own per-record
    // `DBLocker` rather than the shared many-lock.
    //
    // The atomic record set is the same for every monitor event
    // on this PV, so it is collected from the already-sorted
    // `targets` list. `lock_records` itself alias-resolves,
    // deduplicates and sorts, so a record bound by more than one
    // atomic link is locked exactly once.
    let atomic_records: Vec<RecordName> = targets
        .iter()
        .filter(|(_, _, atomic, _)| *atomic)
        .map(|(record, _, _, _)| record.clone())
        .collect();

    // The epoch guard is held only across the atomic group. pvxs
    // scopes `DBManyLocker L(atomic_lock)` to the atomic-target
    // loop and gives non-atomic targets their own per-record
    // `DBLocker` afterwards — so the epoch must be released at the
    // atomic→non-atomic boundary, not at the end of the batch.
    //
    // The two phases are two loops over the same `atomic`-first sorted
    // `targets`, and the split is structural rather than tidiness: the
    // epoch guard is a blocking `!Send` `ManyRecordWriteGuard`, so putting
    // its scope in a block that contains no `.await` is what makes
    // "the epoch is released before any suspension" a fact the compiler
    // checks. The previous shape — one loop, the epoch in a `mut Option`
    // cleared at the boundary — carried the same runtime invariant but
    // could only assert it in a comment, and the borrow checker sees the
    // `Option` as live across the non-atomic `.await` regardless.
    {
        // Atomic phase. Zero `.await`s below this line while the epoch is
        // held.
        let _atomic_epoch = if atomic_records.is_empty() {
            None
        } else {
            Some(db_handle.lock_records(&atomic_records))
        };

        for (record, _order, atomic, passive_only) in &targets {
            // `targets` is sorted atomic-first, so the first non-atomic
            // target ends the atomic group.
            if !*atomic {
                break;
            }
            // The epoch above holds this target's lock.
            scan_target_already_locked(&db_handle, record, *passive_only);
        }
    }

    // Non-atomic phase — reached only after the epoch above went out of
    // scope; pvxs gives each non-atomic target its own `DBLocker`.
    for (record, _order, atomic, passive_only) in &targets {
        if *atomic {
            continue;
        }
        let _held = db_handle.lock_records([record]);
        scan_target_already_locked(&db_handle, record, *passive_only);
    }
}

#[epics_base_rs::async_trait]
impl LinkSet for PvaLinkResolver {
    fn is_connected(&self, name: &[u8]) -> bool {
        // A link that has not been opened reports "not connected"; the
        // resolver hot path or `pvxr` opens it lazily.
        let Some(full) = strip_scheme(name) else {
            return false;
        };
        // resolve THIS link's monitor variant. `link_names()` hands back
        // one identity string per distinct `(pv, pipeline, Q)` variant,
        // so the wait confirms each variant's own monitor connected, not
        // just the first to share the PV name.
        let cfg = self.inp_cfg_for(full);
        match self
            .registry
            .try_get_inp(&cfg.pv_name, cfg.pipeline, cfg.queue_size)
        {
            Some(link) => link.is_connected(),
            None => false,
        }
    }

    async fn get_value(&self, name: &[u8]) -> Option<EpicsValue> {
        if !self.is_enabled() {
            return None;
        }
        let full = strip_scheme(name)?;
        // the per-link config, for the field selector.
        let cfg = self.inp_cfg_for(full);

        // Fast path: cached monitor value, no async runtime touch.
        // resolve THIS caller's monitor variant, then apply the per-link
        // field selector.
        if let Some(link) = self
            .registry
            .try_get_inp(&cfg.pv_name, cfg.pipeline, cfg.queue_size)
            && let Some(value) = link.with_cached_value(&cfg.field, pvfield_to_epics_value)
        {
            self.reads
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            return value;
        }

        // Slow path: open the link / fall back to a fresh GET.
        // B4: use `inp_cfg_for` so a link registered via `open_link`
        // keeps its options (`sevr`, `Q`, `pipeline`, `monorder`);
        // `default_inp_cfg` would discard them.
        let field = cfg.field.clone();
        let value = async {
            let link = self.opener.open(&cfg).await.ok()?;
            link.read_with_field(&field).await.ok()
        }
        .await?;
        self.reads
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        pvfield_to_epics_value(&value)
    }

    /// C `dbCaGetLink` (`dbCa.c:419-506`) — the monitor-fed cache only.
    /// This is [`Self::get_value`]'s fast path (`integration.rs:1155-1166`)
    /// without the `get_or_open` + `read_with_field` slow path, so the
    /// record-processing read cannot suspend on a PVA connect or GET round
    /// trip. pvxs reads the same way: `pvaGetValue` serves the monitor
    /// snapshot the channel task refreshed (`pvxs/ioc/pvalink_lset.cpp:199-240`).
    fn get_cached_value(&self, name: &[u8]) -> Option<EpicsValue> {
        if !self.is_enabled() {
            return None;
        }
        let full = strip_scheme(name)?;
        let cfg = self.inp_cfg_for(full);
        let link = self
            .registry
            .try_get_inp(&cfg.pv_name, cfg.pipeline, cfg.queue_size)?;
        let value = link.with_cached_value(&cfg.field, pvfield_to_epics_value)?;
        self.reads
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        value
    }

    /// `pvaGetValue`'s `!self->valid()` arm, which raises `"%s Disconn"`
    /// (`pvalink_lset.cpp:263-268`); a link never opened is not valid.
    fn get_failure_disconn(&self, name: &[u8]) -> bool {
        if !self.is_enabled() {
            return false;
        }
        let Some(full) = strip_scheme(name) else {
            return false;
        };
        let cfg = self.inp_cfg_for(full);
        !self
            .registry
            .try_get_inp(link_pv_name(full), cfg.pipeline, cfg.queue_size)
            .is_some_and(|link| link.is_valid())
    }

    /// pvxs `pvaOpenLink` (`pvxs/ioc/pvalink_lset.cpp:63-171`), at iocInit
    /// and on every put of the link: the holder is attached under
    /// the caller's hold, and its channel opened on the reactor.
    fn open_link(&self, holder: LinkHolder<'_>, name: &[u8]) -> LinkOpen {
        let Some(full) = strip_scheme(name) else {
            return LinkOpen::Lset;
        };
        let direction = holder_direction(holder.class());
        let cfg = match direction {
            LinkDirection::Inp => self.inp_cfg_for(full),
            LinkDirection::Out => self.out_cfg_for(full),
        };
        let holder = Holder::from(holder);
        if let Some(id) = self.attach(&holder, &cfg) {
            let this = self.clone();
            self.reactor
                .spawn(async move { this.open_attached(holder, cfg, id).await });
        }
        LinkOpen::Lset
    }

    /// pvxs `pvaRemoveLink` (`pvxs/ioc/pvalink_lset.cpp:176-185`).
    fn remove_link(&self, holder: LinkHolder<'_>, name: &[u8]) {
        let Some(full) = strip_scheme(name) else {
            return;
        };
        let cfg = match holder_direction(holder.class()) {
            LinkDirection::Inp => self.inp_cfg_for(full),
            LinkDirection::Out => self.out_cfg_for(full),
        };
        self.detach(&holder.into(), &cfg);
    }

    /// C `dbCaAddLink`'s `CA_CONNECT` (`dbCa.c:393`): open the monitor so
    /// later cached reads have something to serve. Runs on the database's
    /// link work owner, never on a record-processing thread.
    async fn connect_link(
        &self,
        name: &[u8],
        _: Option<epics_base_rs::server::database::CaLinkInit>,
    ) {
        if !self.is_enabled() {
            return;
        }
        let Some(full) = strip_scheme(name) else {
            return;
        };
        let cfg = self.inp_cfg_for(full);
        let _ = self.opener.open(&cfg).await;
    }

    /// C `dbCaPutLinkCallback`'s `if (!pca->isConnected || !pca->hasWriteAccess)
    /// return -1;` (`dbCa.c:529-532`), answered from cached state only — the
    /// database asks this on the record-processing thread, inside the
    /// record's advisory write gate.
    ///
    /// Looks up the **OUT** registry variant, not the INP one
    /// [`LinkSet::is_connected`] uses: the registry keys on direction, so an
    /// OUT-only channel is invisible to `try_get_inp` and the trait's default
    /// gate (derived from `is_connected`) would refuse every OUT write to a
    /// healthy channel. An OUT `PvaLink` tracks its connection through its
    /// own liveness monitor — pvxs runs a monitor on every channel, INP and
    /// OUT alike, to maintain `lchan->connected`
    /// (`pvxs/ioc/pvalink_channel.cpp:342-363`), and gates the write on
    /// `valid() = connected && root` (`pvxs/ioc/pvalink_lset.cpp:609`).
    ///
    /// A link never opened reports `Unopened` so the write is still staged
    /// and `put_value`'s lazy `get_or_open` performs the open; pvxs opens at
    /// link-init instead, so it never sees this state.
    fn put_admission(&self, name: &[u8]) -> PutAdmission {
        if !self.is_enabled() {
            // `put_value` would reject it — refuse at the gate so the owning
            // record alarms in this cycle, as a `-1` from C does.
            return PutAdmission::Refused;
        }
        let Some(full) = strip_scheme(name) else {
            // A `ca://` name reaching the pva lset: `put_value` rejects it.
            return PutAdmission::Refused;
        };
        let cfg = self.out_cfg_for(full);
        // pvxs gates the write on `if(!self->retry && !self->valid())`
        // (`pvxs/ioc/pvalink_lset.cpp:609`): a `retry` link deliberately skips the
        // connection gate and queues for replay on reconnect. Refusing it
        // here would delete that whole mechanism, so a retry link is always
        // admitted and `flush_scratch` owns its disconnect handling.
        if cfg.retry {
            return PutAdmission::Connected;
        }
        match self
            .registry
            .try_get_out(&cfg.pv_name, cfg.pipeline, cfg.queue_size)
        {
            None => PutAdmission::Unopened,
            Some(link) if link.is_connected() => PutAdmission::Connected,
            Some(_) => PutAdmission::Refused,
        }
    }

    async fn put_value(&self, name: &[u8], value: EpicsValue, op: LinkPutOp) -> Result<(), String> {
        if !self.is_enabled() {
            return Err("pvalink disabled".into());
        }
        // An `Async` op marks a put-notify / blocking-put chain: the
        // source record's processing is held until the downstream put
        // completes, so the PUT must carry the block option
        // (`record._options.block`) — pvxs `pvaPutValueAsync`. A plain
        // OUT write is fire-and-forget (`pvaPutValue`, block off).
        let block = matches!(op, LinkPutOp::Async);
        let full = strip_scheme(name).ok_or_else(|| {
            format!(
                "pvalink rejects ca:// scheme: {} (use the CA-link path instead)",
                String::from_utf8_lossy(name)
            )
        })?;
        // the full string keys the per-link OUT config.
        let cfg = self.out_cfg_for(full);
        // bypass the Display→string→parse round-trip for
        // ARRAYS (where Display alloc is O(N_elements * digits) and
        // pvput re-parses 25 MB strings on a 1 M-element waveform).
        // SCALARS keep the string path so the text is coerced against
        // the channel's introspected scalar type.
        //
        // classify via `is_array_value` — an
        // exhaustive match with no wildcard arm — so a future
        // `EpicsValue` array variant cannot silently miss this gate.
        // The earlier inline `matches!` over a hard-coded subset
        // omitted `Int64Array` (never covered) and `UInt64Array`
        // (added with `DBF_UINT64` but never wired in), routing those
        // through the string PUT path where the bracketed `Display`
        // text is unparseable.
        let array_path = is_array_value(&value);
        async {
            // Resolve the SHARED channel owner for this PV (the
            // registry no longer keys on the per-link OUT options),
            // then stage THIS link's write with its own
            // `field`/`proc`/`defer`/`retry` so sibling fields
            // coalesce into one upstream PUT.
            let link = self.opener.open(&cfg).await.map_err(|e| e.to_string())?;
            if array_path {
                let pv_field = crate::convert::epics_to_pv_field(&value);
                link.put_out_field(&cfg, &pv_field, block)
                    .await
                    .map_err(|e| e.to_string())
            } else {
                let value_str = value.to_string();
                link.put_out_str(&cfg, &value_str, block)
                    .await
                    .map_err(|e| e.to_string())
            }
        }
        .await
    }

    /// pvxs `pvaOpenLink` for a forward link: the options registered and
    /// the config resolved once, and the channel open staged off the record
    /// thread (C's `CA_CONNECT`), so a `PvaForward` dispatch only gates
    /// and signals.
    fn forward_handle(&self, name: &[u8]) -> Box<dyn LinkForward> {
        let Some(full) = strip_scheme(name) else {
            eprintln!(
                "pvalink rejects ca:// scheme: {} (use the CA-link path instead)",
                String::from_utf8_lossy(name)
            );
            return Box::new(RefusedForward);
        };
        // A forward link shares the monitor-backed channel a value INP
        // link would open (pvxs's `pvaLinkChannel` always monitors —
        // `pvxs/ioc/pvalink_channel.cpp`), which is what `is_connected`
        // checks.
        let cfg = self.inp_cfg_for(full);
        let shared = Arc::new(ForwardShared {
            enabled: self.enabled.clone(),
            retry: cfg.retry,
            link: arc_swap::ArcSwapOption::empty(),
            pending: std::sync::atomic::AtomicBool::new(false),
            wake: tokio::sync::Notify::new(),
            closed: std::sync::atomic::AtomicBool::new(false),
        });
        self.reactor.spawn(run_forward(
            shared.clone(),
            self.opener.clone(),
            PvaLinkConfig::clone(&cfg),
        ));
        Box::new(PvaForward(shared))
    }

    async fn flush_puts(&self) {
        if !self.is_enabled() {
            return;
        }
        // Production drain of any retry-queued OUT writes: re-attempt
        // every shared channel owner that a prior disconnect left with
        // a `retry` write staged, now that some OUT activity has reached
        // the lset (the upstream may have reconnected). `flush_retry_pending`
        // is a no-op on channels with nothing stuck, so a freshly-`defer`red
        // write awaiting its sibling is never flushed early.
        let links = self.registry.out_links();
        if links.is_empty() {
            return;
        }
        async {
            for link in links {
                let _ = link.flush_retry_pending().await;
            }
        }
        .await;
    }

    fn alarm_message(&self, name: &[u8]) -> Option<Arc<str>> {
        // parse the caller's full link config and apply the
        // caller's own `sevr` mode — `get_or_open` may return a
        // previously cached INP link whose `config.sevr` belongs to a
        // different caller, so `default_inp_cfg(bare)` would discard
        // this caller's `?sevr=` option. pvxs `pvaLinkConfig::sevr`
        // is per-link (`pvxs/ioc/pvalink.h:65`).
        let full = strip_scheme(name)?;
        let cfg = self.inp_cfg_for(full);
        let sevr = cfg.sevr;
        let field = &cfg.field;
        // Cache-only: C's `getAttributes` family reads `pca->...` and never creates
        // a channel (`dbCa.c:633-675`); pvxs's metadata getters read the cached NT
        // value under the channel lock (`pvxs/ioc/pvalink_lset.cpp:199-257`). The
        // open is staged by the value-read path / `connect_link` on the link work
        // owner, never here — this runs on the record thread inside the record's
        // advisory write gate.
        let link = self
            .registry
            .try_get_inp(&cfg.pv_name, cfg.pipeline, cfg.queue_size)?;
        link.alarm_message_with(field, sevr)
    }

    fn alarm_severity(&self, name: &[u8]) -> Option<i32> {
        // B2: surface the gated link-alarm severity so the owning
        // record's `LINK_ALARM` actually reflects the remote PV's
        // alarm. The `MS`/`NMS`/`MSI` gate is applied here.
        //
        // apply the caller's own `sevr` mode (parsed from the
        // caller's full link config), not the cached link's shared
        // `config.sevr`.
        let full = strip_scheme(name)?;
        let cfg = self.inp_cfg_for(full);
        let sevr = cfg.sevr;
        let field = &cfg.field;
        // Cache-only: C's `getAttributes` family reads `pca->...` and never creates
        // a channel (`dbCa.c:633-675`); pvxs's metadata getters read the cached NT
        // value under the channel lock (`pvxs/ioc/pvalink_lset.cpp:199-257`). The
        // open is staged by the value-read path / `connect_link` on the link work
        // owner, never here — this runs on the record thread inside the record's
        // advisory write gate.
        let link = self
            .registry
            .try_get_inp(&cfg.pv_name, cfg.pipeline, cfg.queue_size)?;
        link.link_alarm_severity_with(field, sevr)
    }

    fn remote_alarm(&self, name: &[u8]) -> Option<epics_base_rs::server::database::RemoteAlarm> {
        // Ungated remote alarm snapshot for DB-link inspection
        // (`dbGetAlarm`/`dbGetAlarmMsg`). Unlike `alarm_severity`, this
        // applies NO `sevr` gate — a default `NMS` link still reports
        // its remote severity here. pvxs `pvaGetAlarmMsg`
        // (`pvxs/ioc/pvalink_lset.cpp:542-569`) reads the snapshot directly. The
        // per-link `field` still selects the metadata root, matching the
        // value/alarm getters.
        let full = strip_scheme(name)?;
        let cfg = self.inp_cfg_for(full);
        let field = &cfg.field;
        // Cache-only: C's `getAttributes` family reads `pca->...` and never creates
        // a channel (`dbCa.c:633-675`); pvxs's metadata getters read the cached NT
        // value under the channel lock (`pvxs/ioc/pvalink_lset.cpp:199-257`). The
        // open is staged by the value-read path / `connect_link` on the link work
        // owner, never here — this runs on the record thread inside the record's
        // advisory write gate.
        let link = self
            .registry
            .try_get_inp(&cfg.pv_name, cfg.pipeline, cfg.queue_size)?;
        link.remote_alarm_snapshot(field)
    }

    fn time_stamp(&self, name: &[u8]) -> Option<(i64, i32, u64)> {
        let full = strip_scheme(name)?;
        // prefer the operator-installed link config so
        // `?time=true` gates the lookup; fall back to defaults for
        // bare auto-resolved links (which never adopt upstream time —
        // matching pvxs `pvaLinkConfig::time` default false).
        //
        // gate on the caller's own parsed `cfg.time`, not the
        // cached link's `link.config().time`. `get_or_open` can
        // return a previously cached INP link whose `time` flag
        // belongs to a different caller; reading `link.config().time`
        // would adopt or drop the upstream timestamp based on the
        // wrong link's option. pvxs `pvaLinkConfig::time` is per-link.
        let cfg = self.inp_cfg_for(full);
        let want_time = cfg.time;
        let field = &cfg.field;
        // Cache-only: C's `getAttributes` family reads `pca->...` and never creates
        // a channel (`dbCa.c:633-675`); pvxs's metadata getters read the cached NT
        // value under the channel lock (`pvxs/ioc/pvalink_lset.cpp:199-257`). The
        // open is staged by the value-read path / `connect_link` on the link work
        // owner, never here — this runs on the record thread inside the record's
        // advisory write gate.
        let link = self
            .registry
            .try_get_inp(&cfg.pv_name, cfg.pipeline, cfg.queue_size)?;
        // only adopt the upstream timestamp when this caller's
        // link was configured with `time=true`. pvxs
        // `pvxs/ioc/pvalink_lset.cpp:433-435` copies the latched remote NT
        // timestamp into the owning record only when this flag is
        // set; otherwise the owning record keeps its locally-stamped
        // processing time.
        if !want_time {
            return None;
        }
        link.time_stamp(field)
    }

    fn link_metadata(&self, name: &[u8]) -> Option<epics_base_rs::server::database::LinkMetadata> {
        // surface the remote display/control/valueAlarm
        // metadata, DBF type and element count through the DB link
        // API, mirroring the pvxs pvalink lset metadata getter set —
        // the `&pvaGetControlLimits` … `&pvaGetUnits` slots of
        // `pva_lset` (`pvxs/ioc/pvalink_lset.cpp:715-719`). Reads the
        // cached NT value — no fresh GET — exactly as the pvxs
        // getters read `fld_meta` / `fld_value` under the channel
        // lock.
        //
        // strip the query from the remote PV name (a
        // query-bearing name would otherwise be opened as a literal
        // PV name including `?...`) and apply the caller's own parsed
        // `field` so DBF type / element count come from this link's
        // sub-field, not whichever cached INP link is shared. pvxs
        // `pvaGetDBFtype` reads the per-link `fld_value`
        // (`pvxs/ioc/pvalink_lset.cpp:199`).
        let full = strip_scheme(name)?;
        let cfg = self.inp_cfg_for(full);
        let field = &cfg.field;
        // Cache-only: C's `getAttributes` family reads `pca->...` and never creates
        // a channel (`dbCa.c:633-675`); pvxs's metadata getters read the cached NT
        // value under the channel lock (`pvxs/ioc/pvalink_lset.cpp:199-257`). The
        // open is staged by the value-read path / `connect_link` on the link work
        // owner, never here — this runs on the record thread inside the record's
        // advisory write gate.
        let link = self
            .registry
            .try_get_inp(&cfg.pv_name, cfg.pipeline, cfg.queue_size)?;
        link.link_metadata_with(field)
    }

    fn link_names(&self) -> Vec<epics_base_rs::types::RecordName> {
        // The [`LinkSet::link_names`] enumeration contract: surface one
        // identity per opened INP monitor variant, for link introspection.
        // NOT consumed by `dbCaRun`'s wait, which is CA-facility only;
        // pvalink never blocks iocInit (pvxs parity). A `pva://` monitor
        // connects in the background.
        //
        // Each name is a canonical link string that round-trips back to
        // the same `(pv_name, pipeline, queue_size)` variant when fed to
        // `is_connected(name)`: a bare PV name for the default variant, or
        // `PV?pipeline=…&Q=…` for a non-default one. Two records on the
        // same PV with different `Q` / `pipeline` therefore each get their
        // own identity, and the per-name `is_connected` query lands on
        // that record's own monitor. OUT links are excluded: they install
        // no monitor and have no connection signal to report.
        let default_q = PvaLinkConfig::defaults_for("", LinkDirection::Inp).queue_size;
        self.registry
            .inp_identities()
            .into_iter()
            .map(|(pv, pipeline, qsize)| {
                if !pipeline && qsize == default_q {
                    pv.into()
                } else {
                    format!("{pv}?pipeline={pipeline}&Q={qsize}").into()
                }
            })
            .collect()
    }
}

/// Strip the `pva://` scheme prefix the bridge sometimes prepends.
/// Pvalink only handles PVA — `ca://` is the libca scheme and is
/// dispatched by the CA-link path elsewhere, so an explicit `ca://`
/// here returns `None` so the caller can short-circuit, as does a name
/// that is not UTF-8. Names with no scheme are passed through.
///
/// NOTE: the returned string may still contain a `?` query part when
/// the link was parsed from a JSON object with extra options.
/// Call `strip_query` on the result before using it as a registry key
/// or a PV name lookup.
/// A `pva` forward link, opened by [`PvaLinkResolver::forward_handle`].
///
/// C `dbCa.c:614-619` `scanForward`: the FWD link is a
/// `dbCaPutLink(plink, DBR_SHORT, &fwdLinkValue, 1)`, i.e. it stages a
/// `CA_WRITE_NATIVE` action and **returns** — the upstream round trip is the
/// `dbCaTask`'s work, never the record thread's. Here the staged action is
/// `pending`, and the link's own worker ([`run_forward`]) is the `dbCaTask`.
/// Forwards staged before the worker runs coalesce into one put, as pvxs's
/// `put(true)` cancels a put still in progress
/// (`pvxs/ioc/pvalink_channel.cpp:220-280`).
///
/// The gate before staging is C's
/// `if (!pca->isConnected || !pca->hasWriteAccess) return -1`
/// (`dbCa.c:529-532`), which pvxs surfaces as `pvaScanForward`'s
/// `if(!self->retry && !self->valid())` (`pvxs/ioc/pvalink_lset.cpp:677`); a
/// link not yet open is refused too, as a cache miss on the value-read path
/// is. A refusal is LINK/INVALID on the source record — pvxs's
/// `recGblSetSevrMsg(..., "Disconn")` at `:678`. A failure of the put itself
/// stays a console diagnostic and raises no alarm, because pvxs's is one
/// too: `linkPutDone` reports it with `errlogPrintf` and carries an explicit
/// `// TODO: signal INVALID_ALARM ?` (`pvxs/ioc/pvalink_channel.cpp:192-195`).
struct PvaForward(Arc<ForwardShared>);

struct ForwardShared {
    enabled: Arc<std::sync::atomic::AtomicBool>,
    /// This link's own, pvxs `self->retry`: the channel is shared by every
    /// link to the PV, so its config holds whichever opened it first.
    retry: bool,
    /// Stored once by the worker when the channel opens.
    link: arc_swap::ArcSwapOption<PvaLink>,
    pending: std::sync::atomic::AtomicBool,
    wake: tokio::sync::Notify,
    /// The handle dropped: the worker exits.
    closed: std::sync::atomic::AtomicBool,
}

impl LinkForward for PvaForward {
    fn scan_forward(&self) -> Result<(), LinkForwardRefused> {
        use std::sync::atomic::Ordering;
        let shared = &*self.0;
        if !shared.enabled.load(Ordering::Relaxed) {
            return Err(LinkForwardRefused);
        }
        let guard = shared.link.load();
        let Some(link) = &*guard else {
            // The open is still in flight or failed: the worker retries it.
            shared.wake.notify_one();
            return Err(LinkForwardRefused);
        };
        // A retry link falls through and the client holds the put until the
        // channel connects.
        if !shared.retry && !link.is_connected() {
            return Err(LinkForwardRefused);
        }
        // A forward already pending has its wake on the way.
        if !shared.pending.swap(true, Ordering::AcqRel) {
            shared.wake.notify_one();
        }
        Ok(())
    }
}

impl Drop for PvaForward {
    fn drop(&mut self) {
        self.0
            .closed
            .store(true, std::sync::atomic::Ordering::Release);
        self.0.wake.notify_one();
    }
}

/// A forward link the resolver cannot open: every forward is refused.
struct RefusedForward;

impl LinkForward for RefusedForward {
    fn scan_forward(&self) -> Result<(), LinkForwardRefused> {
        Err(LinkForwardRefused)
    }
}

/// [`PvaForward`]'s worker: opens the channel, then puts once per wake that
/// found a forward staged.
async fn run_forward(shared: Arc<ForwardShared>, opener: ChannelOpener, cfg: PvaLinkConfig) {
    use std::sync::atomic::Ordering;
    loop {
        if shared.closed.load(Ordering::Acquire) {
            return;
        }
        let link = match shared.link.load_full() {
            Some(link) => link,
            None => match opener.open(&cfg).await {
                Ok(link) => {
                    shared.link.store(Some(link.clone()));
                    link
                }
                // Retried on the next forward's wake.
                Err(_) => {
                    shared.wake.notified().await;
                    continue;
                }
            },
        };
        while shared.pending.swap(false, Ordering::AcqRel) {
            if let Err(e) = link.scan_forward(cfg.retry).await {
                // pvxs `linkPutDone`: `errlogPrintf("%s PVA link put ERROR:
                // %s\n", key.first, e.what())` — named channel, console
                // only. `eprintln!` is this crate's errlog equivalent (see
                // `report_open_failure`): the RTEMS target installs no
                // `tracing` subscriber.
                eprintln!("{} PVA forward link put ERROR: {e}", cfg.pv_name);
            }
        }
        shared.wake.notified().await;
    }
}

fn strip_scheme(name: &[u8]) -> Option<&str> {
    // PVA names are text until the PVA name layer.
    let name = std::str::from_utf8(name).ok()?;
    if let Some(stripped) = name.strip_prefix("pva://") {
        return Some(stripped);
    }
    if name.starts_with("ca://") {
        return None;
    }
    Some(name)
}

/// Strip the `?key=value&…` query part appended to bare PV
/// names when a JSON `{pva: {pv: …, field: …}}` link carries options.
/// Returns the bare PV name before the first `?`, or the whole slice
/// if there is no `?`.
fn strip_query(s: &str) -> &str {
    s.split_once('?').map_or(s, |(bare, _)| bare)
}

/// Extract the bare upstream PV identity from a pvalink link body,
/// dropping ALL THREE representations of per-link options:
///   * the structured-JSON identity key `PV<SEP>k=kind:v<SEP>…` minted by
///     `epics_base_rs::server::record::pvajson_identity_key` (the byte is
///     [`PVAJSON_IDENTITY_SEP`]), and
///   * the `?key=value&…` query part (convenience-URI form), and
///   * the legacy whitespace-separated trailing modifiers
///     (`PP`/`NPP`/`CP`/`CPP`/`MS`/`MSI`/`MSS`/`NMS` — the DBD suffix
///     form a record writes as `pva://TARGET MS`).
///
/// `strip_query` alone left the legacy suffix attached, so
/// `"TARGET MS"` became the registry channel identity and the
/// `default_inp_cfg` pv_name — the modifier was opened as part of the
/// remote name. A PV name never contains whitespace, so the first token
/// after query-stripping is always the PV; `PvaLinkConfig::parse` (via
/// `strip_legacy_mods`) remains the single owner that interprets the
/// trailing modifiers into the per-link config. A structured link's
/// channel is the `pv_name` of the config its key decodes to, never this
/// prefix, which holds the `pv` escaped. This collapses all option
/// representations to one PV identity uniformly, so the channel is shared
/// by bare PV + `(pipeline, Q)` regardless of how the options were
/// expressed.
fn link_pv_name(s: &str) -> &str {
    let bare = s.split(PVAJSON_IDENTITY_SEP).next().unwrap_or(s);
    let no_query = strip_query(bare);
    no_query.split_whitespace().next().unwrap_or(no_query)
}

/// A link's config as decided the first time its key resolved, shared with
/// every later lset call so none of them builds, parses or clones one.
type LinkCfg = Arc<PvaLinkConfig>;

type LinkCfgMap = parking_lot::RwLock<std::collections::HashMap<String, LinkCfg>>;

/// A config as the lset keeps it: every INP link monitors, as pvxs's
/// `pvaLinkChannel` always does (`pvxs/ioc/pvalink_channel.cpp`).
fn entry(cfg: PvaLinkConfig, direction: LinkDirection) -> Arc<PvaLinkConfig> {
    Arc::new(match direction {
        LinkDirection::Inp => PvaLinkConfig {
            monitor: true,
            ..cfg
        },
        LinkDirection::Out => cfg,
    })
}

/// The config of the link `full` names in `direction`, decided the first
/// time `full` resolves and kept under it: a structured key's options, a
/// convenience URI's `?query`, the options registered for the bare PV, else
/// pvxs's defaults.
///
/// keying by full string ensures two links to the same PV
/// with different options each return their own config
/// (`pvxs/ioc/pvalink.h:65` per-link `pvaLinkConfig`).
fn link_cfg(link_options: &LinkCfgMap, full: &str, direction: LinkDirection) -> LinkCfg {
    if let Some(cfg) = link_options.read().get(full) {
        return cfg.clone();
    }
    let bare = link_pv_name(full);
    let parsed = || {
        (full != bare)
            .then(|| PvaLinkConfig::parse(&format!("pva://{full}"), direction).ok())
            .flatten()
            .map(|cfg| entry(cfg, direction))
    };
    let registered = || link_options.read().get(bare).cloned();
    let cfg = json_link_cfg(full, direction)
        .or_else(parsed)
        .or_else(registered)
        .unwrap_or_else(|| entry(PvaLinkConfig::defaults_for(bare, direction), direction));
    link_options
        .write()
        .entry(full.to_string())
        .or_insert(cfg)
        .clone()
}

/// A structured link's config, built from the options its identity key
/// carries (pvxs `pvaOpenLink` gets the link's own `jlink`); `None` for a
/// convenience URI or a bare name. Every key a record's link produces reads
/// back, an empty name's included: the loader refuses the separator-bearing
/// plain name that would not (`check_link_text`).
fn json_link_cfg(key: &str, direction: LinkDirection) -> Option<LinkCfg> {
    if !key.contains(PVAJSON_IDENTITY_SEP) {
        return None;
    }
    let (pv, options) = pvajson_identity_parts(key)?;
    PvaLinkConfig::from_jlink_options(&pv, &options, direction)
        .ok()
        .map(|cfg| entry(cfg, direction))
}

/// True iff `value` is an array `EpicsValue` variant — the pvalink
/// OUT dispatcher routes these through the typed `PvField` write path
/// (`crate::convert::epics_to_pv_field` → `PvaLink::write_pv_field`)
/// instead of the scalar `Display`→string→`pvput`-parse path.
///
/// This is the single classification gate for the
/// OUT typed-array path. It is an EXHAUSTIVE match with no wildcard
/// arm — every `EpicsValue` variant is named, so adding a future
/// array variant to `EpicsValue` forces a compile error here until
/// it is classified, which structurally prevents another
/// missed-gate regression.
fn is_array_value(value: &EpicsValue) -> bool {
    match value {
        EpicsValue::ShortArray(_)
        | EpicsValue::FloatArray(_)
        | EpicsValue::EnumArray(_)
        | EpicsValue::DoubleArray(_)
        | EpicsValue::LongArray(_)
        | EpicsValue::CharArray(_)
        | EpicsValue::Int64Array(_)
        | EpicsValue::UInt64Array(_)
        | EpicsValue::UShortArray(_)
        | EpicsValue::ULongArray(_)
        | EpicsValue::UCharArray(_)
        | EpicsValue::StringArray(_) => true,
        EpicsValue::String(_)
        | EpicsValue::Short(_)
        | EpicsValue::Float(_)
        | EpicsValue::Enum(_)
        // Transient NTEnum carrier is a scalar (single index + choices), not
        // an array — routes through the scalar Display path like `Enum`.
        | EpicsValue::EnumWithChoices { .. }
        | EpicsValue::Char(_)
        | EpicsValue::Long(_)
        | EpicsValue::Double(_)
        | EpicsValue::Int64(_)
        | EpicsValue::UInt64(_)
        | EpicsValue::UShort(_)
        | EpicsValue::ULong(_)
        | EpicsValue::UChar(_) => false,
    }
}

/// Best-effort conversion. We coerce scalar values and 1-D scalar arrays;
/// structures collapse to their `value` field. Returns `None` for
/// unsupported shapes — callers fall back to `None` in the resolver
/// closure, which surfaces as "no link value" upstream (record alarm
/// LINK/INVALID).
fn pvfield_to_epics_value(field: &PvField) -> Option<EpicsValue> {
    // Follow a selected union / non-empty variant to its concrete member
    // before conversion — pvxs `value.lookup("->")` for an Any/Union
    // value (`pvxs/ioc/pvalink_lset.cpp:278-279`). The standard NTNDArray carries
    // its pixels in a discriminated union `value` member; without this an
    // INP pvalink pointed at an NTNDArray reported no usable value.
    // Idempotent on plain scalars/arrays/structures.
    match field.deref_selected() {
        PvField::Scalar(sv) => Some(scalar_to_epics(sv)),
        PvField::Structure(s) => {
            // NTEnum (`pvxs/ioc/pvalink_lset.cpp:331`, gated on
            // `value.id()=="enum_t"`): the value lives in the `index`
            // int of the `enum_t` sub-structure, never a `value`
            // scalar. The generic `value`-child collapse below would
            // recurse into the `enum_t` struct, find no `value` child,
            // and return `None` — leaving a record with a bare
            // `pva://enumPV` INP (the default `field="value"`) stuck
            // LINK/INVALID. Mirror pvxs `pvaGetValue`: read
            // `value["index"]` as the enum index. The recursion reaches
            // the `enum_t` struct here as its own `Structure` arm, so
            // the check is uniform whether the caller hands us the NT
            // wrapper or the `enum_t` directly.
            if s.struct_id == "enum_t" {
                if let Some(PvField::Scalar(sv)) = s.get_field("index") {
                    let index = match scalar_to_epics(sv) {
                        EpicsValue::Enum(v) => v,
                        other => other.to_f64().map(|f| f as u16).unwrap_or(0),
                    };
                    // pvxs `pvaGetValue` reads the sibling `choices` string[]
                    // alongside `index` (`pvxs/ioc/pvalink_lset.cpp:344-356`)
                    // so a later type-aware `put_field` can store the label on
                    // a DBR_STRING target or the index on a numeric one. The
                    // resolver boundary is dbrType-blind, so carry both
                    // through `EnumWithChoices`; an absent/empty `choices`
                    // makes the string-target path fall back to the "%u" index
                    // form.
                    let choices = match s.get_field("choices") {
                        Some(PvField::ScalarArray(arr)) => arr
                            .iter()
                            .filter_map(|sv| match sv {
                                ScalarValue::String(s) => Some(s.clone()),
                                _ => None,
                            })
                            .collect(),
                        _ => Vec::new(),
                    };
                    return Some(EpicsValue::EnumWithChoices { index, choices });
                }
            }
            for (name, sub) in &s.fields {
                if name == "value" {
                    return pvfield_to_epics_value(sub);
                }
            }
            None
        }
        PvField::ScalarArray(arr) => {
            // `pvxs/ioc/pvalink_lset.cpp:287` handles every pvData
            // scalar-array variant (signed/unsigned 8/16/32/64-bit,
            // float32/float64, bool, string). Mirror that coverage so
            // an INP pvalink can read any waveform the upstream serves.
            // Mixed-element arrays are unusual on the wire — pvData
            // ScalarArray is homogeneous — but if a producer hands us a
            // Vec<ScalarValue> of mixed variants we filter to the
            // first-element kind and quietly drop the others (matches
            // pvxs's "skip type-mismatched elements" behaviour).
            let first = arr.first()?;
            match first {
                ScalarValue::Double(_) => Some(EpicsValue::DoubleArray(
                    arr.iter()
                        .filter_map(|s| {
                            if let ScalarValue::Double(d) = s {
                                Some(*d)
                            } else {
                                None
                            }
                        })
                        .collect(),
                )),
                ScalarValue::Float(_) => Some(EpicsValue::FloatArray(
                    arr.iter()
                        .filter_map(|s| {
                            if let ScalarValue::Float(f) = s {
                                Some(*f)
                            } else {
                                None
                            }
                        })
                        .collect(),
                )),
                ScalarValue::Int(_) => Some(EpicsValue::LongArray(
                    arr.iter()
                        .filter_map(|s| {
                            if let ScalarValue::Int(i) = s {
                                Some(*i)
                            } else {
                                None
                            }
                        })
                        .collect(),
                )),
                // a remote `long[]` is 64-bit per element;
                // preserve the full width as `Int64Array` instead of
                // truncating each element to i32.
                ScalarValue::Long(_) => Some(EpicsValue::Int64Array(
                    arr.iter()
                        .filter_map(|s| {
                            if let ScalarValue::Long(l) = s {
                                Some(*l)
                            } else {
                                None
                            }
                        })
                        .collect(),
                )),
                ScalarValue::Short(_) => Some(EpicsValue::ShortArray(
                    arr.iter()
                        .filter_map(|s| {
                            if let ScalarValue::Short(v) = s {
                                Some(*v)
                            } else {
                                None
                            }
                        })
                        .collect(),
                )),
                ScalarValue::UShort(_) => Some(EpicsValue::ShortArray(
                    arr.iter()
                        .filter_map(|s| {
                            if let ScalarValue::UShort(v) = s {
                                Some(*v as i16)
                            } else {
                                None
                            }
                        })
                        .collect(),
                )),
                // a remote `uint[]` is unsigned 32-bit and the
                // link advertises DBF_ULONG metadata; the prior
                // `LongArray` (i32) mapping sign-cast every element
                // above i32::MAX, contradicting that metadata. Preserve
                // the unsigned value losslessly as `Int64Array`, the
                // same width-preserving contract `long[] => Int64Array`
                // / `ulong[] => UInt64Array` use here (pvxs routes
                // DBR_ULONG arrays through `ArrayType::UInt32`,
                // `pvxs/ioc/pvalink_lset.cpp:287-328`).
                ScalarValue::UInt(_) => Some(EpicsValue::Int64Array(
                    arr.iter()
                        .filter_map(|s| {
                            if let ScalarValue::UInt(v) = s {
                                Some(*v as i64)
                            } else {
                                None
                            }
                        })
                        .collect(),
                )),
                // a remote `ulong[]` is 64-bit per element;
                // preserve the full width as `UInt64Array` instead of
                // truncating each element to i32.
                ScalarValue::ULong(_) => Some(EpicsValue::UInt64Array(
                    arr.iter()
                        .filter_map(|s| {
                            if let ScalarValue::ULong(v) = s {
                                Some(*v)
                            } else {
                                None
                            }
                        })
                        .collect(),
                )),
                // pvData `pvByte` is signed 8-bit; widen to Short so the
                // negative range survives the DBF_CHAR-as-signed gap.
                ScalarValue::Byte(_) => Some(EpicsValue::ShortArray(
                    arr.iter()
                        .filter_map(|s| {
                            if let ScalarValue::Byte(v) = s {
                                Some(*v as i16)
                            } else {
                                None
                            }
                        })
                        .collect(),
                )),
                // pvData `pvUByte` is unsigned 8-bit — maps to DBF_CHAR
                // (also stored as u8). Keep the raw octets.
                ScalarValue::UByte(_) => Some(EpicsValue::CharArray(
                    arr.iter()
                        .filter_map(|s| {
                            if let ScalarValue::UByte(v) = s {
                                Some(*v)
                            } else {
                                None
                            }
                        })
                        .collect(),
                )),
                ScalarValue::String(_) => Some(EpicsValue::StringArray(
                    arr.iter()
                        .filter_map(|s| {
                            if let ScalarValue::String(v) = s {
                                Some(v.clone())
                            } else {
                                None
                            }
                        })
                        .collect(),
                )),
                ScalarValue::Boolean(_) => Some(EpicsValue::LongArray(
                    arr.iter()
                        .filter_map(|s| {
                            if let ScalarValue::Boolean(v) = s {
                                Some(if *v { 1 } else { 0 })
                            } else {
                                None
                            }
                        })
                        .collect(),
                )),
            }
        }
        // also support `ScalarArrayTyped` — the wire decoder
        // produces typed arrays (`TypedScalarArray::Double(Arc<[f64]>)`
        // etc.) for performance, and an INP pvalink that read a
        // typed-fast-path waveform previously hit the catch-all and
        // returned `None`. Map each variant onto its `EpicsValue`
        // counterpart.
        PvField::ScalarArrayTyped(arr) => {
            use epics_pva_rs::pvdata::TypedScalarArray;
            match arr {
                TypedScalarArray::Double(a) => Some(EpicsValue::DoubleArray(a.to_shared())),
                TypedScalarArray::Float(a) => Some(EpicsValue::FloatArray(a.to_shared())),
                TypedScalarArray::Int(a) => Some(EpicsValue::LongArray(a.to_shared())),
                // a remote `long[]` is 64-bit per element;
                // preserve the full width as `Int64Array`.
                TypedScalarArray::Long(a) => Some(EpicsValue::Int64Array(a.to_shared())),
                TypedScalarArray::Short(a) => Some(EpicsValue::ShortArray(a.to_shared())),
                TypedScalarArray::UShort(a) => Some(EpicsValue::ShortArray(
                    a.iter().map(|v| *v as i16).collect(),
                )),
                // unsigned 32-bit; widen losslessly to `Int64Array`
                // to agree with the link's DBF_ULONG metadata (same
                // reasoning as the generic `uint[]` arm above).
                TypedScalarArray::UInt(a) => Some(EpicsValue::Int64Array(
                    a.iter().map(|v| *v as i64).collect(),
                )),
                // a remote `ulong[]` is 64-bit per element;
                // preserve the full width as `UInt64Array`.
                TypedScalarArray::ULong(a) => Some(EpicsValue::UInt64Array(a.to_shared())),
                TypedScalarArray::Byte(a) => Some(EpicsValue::ShortArray(
                    a.iter().map(|v| *v as i16).collect(),
                )),
                TypedScalarArray::UByte(a) => Some(EpicsValue::CharArray(a.to_shared())),
                TypedScalarArray::String(a) => Some(EpicsValue::StringArray(a.to_shared())),
                TypedScalarArray::Boolean(a) => Some(EpicsValue::LongArray(
                    a.iter().map(|v| if *v { 1 } else { 0 }).collect(),
                )),
            }
        }
        _ => None,
    }
}

fn scalar_to_epics(sv: &ScalarValue) -> EpicsValue {
    match sv {
        ScalarValue::Double(v) => EpicsValue::Double(*v),
        ScalarValue::Float(v) => EpicsValue::Float(*v),
        // a remote PVA `long` / `ulong` is 64-bit. Mapping it
        // to `EpicsValue::Long` (i32) silently drops the upper 32
        // bits before the local database can coerce it to the
        // destination field type. Preserve the full width as
        // `EpicsValue::Int64` / `EpicsValue::UInt64` — the same
        // typed-conversion contract QSRV uses (`convert.rs`
        // `DbFieldType::Int64 => EpicsValue::Int64`,
        // `DbFieldType::UInt64 => EpicsValue::UInt64`). The owning
        // record's coercion then narrows if its field is 32-bit.
        ScalarValue::Long(v) => EpicsValue::Int64(*v),
        ScalarValue::Int(v) => EpicsValue::Long(*v),
        ScalarValue::Short(v) => EpicsValue::Short(*v),
        ScalarValue::Byte(v) => EpicsValue::Char(*v as u8),
        ScalarValue::ULong(v) => EpicsValue::UInt64(*v),
        // a remote PVA `uint` is unsigned 32-bit and the link
        // advertises DBF_ULONG metadata (`link.rs` `ScalarValue::UInt
        // => LinkDbfType::ULong`, mirroring pvxs `pvaGetDBFtype`
        // `TypeCode::UInt32 => DBF_ULONG`, `pvxs/ioc/pvalink_lset.cpp:222`).
        // Carrying it as `EpicsValue::Long` (i32) sign-casts any value
        // above i32::MAX (e.g. 0x8000_0000 → -2147483648), so the
        // record would consume a negative value the unsigned metadata
        // never promised. Widen losslessly to `Int64`, matching the
        // `Long => Int64` / `ULong => UInt64` width-preserving contract
        // above; the owning record narrows to its field type.
        ScalarValue::UInt(v) => EpicsValue::Int64(*v as i64),
        ScalarValue::UShort(v) => EpicsValue::Short(*v as i16),
        // DBF_CHAR is signed (pvByte). Widen UByte to Short so the
        // unsigned 128..255 range survives the cross-protocol hop.
        ScalarValue::UByte(v) => EpicsValue::Short(*v as i16),
        ScalarValue::Boolean(v) => EpicsValue::Long(if *v { 1 } else { 0 }),
        ScalarValue::String(s) => EpicsValue::String(s.clone()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `record`'s `INP` as a link holder.
    fn inp(record: &str) -> LinkHolder<'_> {
        LinkHolder::new("ai", record.as_bytes(), "INP")
    }

    /// `record`'s `OUT` as a link holder.
    fn out(record: &[u8]) -> LinkHolder<'_> {
        LinkHolder::new("ao", record, "OUT")
    }

    fn default_inp_cfg(pv_name: &str) -> PvaLinkConfig {
        PvaLinkConfig {
            monitor: true,
            ..PvaLinkConfig::defaults_for(pv_name, LinkDirection::Inp)
        }
    }

    #[test]
    fn pvfield_scalar_to_epics_double() {
        let f = PvField::Scalar(ScalarValue::Double(2.5));
        assert_eq!(pvfield_to_epics_value(&f), Some(EpicsValue::Double(2.5)));
    }

    #[test]
    fn pvfield_struct_with_value_extracts() {
        use epics_pva_rs::pvdata::PvStructure;
        let mut s = PvStructure::new("epics:nt/NTScalar:1.0");
        s.fields
            .push(("value".into(), PvField::Scalar(ScalarValue::Long(42))));
        let f = PvField::Structure(s);
        // a remote PVA `long` is 64-bit — it now maps to
        // `EpicsValue::Int64`, not a truncated `EpicsValue::Long`.
        assert_eq!(pvfield_to_epics_value(&f), Some(EpicsValue::Int64(42)));
    }

    #[test]
    fn pvfield_ntenum_extracts_index() {
        use epics_pva_rs::pvdata::PvStructure;
        // An NTEnum's `value` child is an `enum_t` carrying the choice
        // in its `index` int, with no `value` scalar of its own. A bare
        // `pva://enumPV` INP (default `field="value"`) lands here.
        // Before the enum_t branch the generic value-collapse recursed
        // into `enum_t`, found no `value`, and returned `None` →
        // record LINK/INVALID. pvxs `pvaGetValue` reads `value["index"]`.
        let mut enum_t = PvStructure::new("enum_t");
        enum_t
            .fields
            .push(("index".into(), PvField::Scalar(ScalarValue::Int(2))));
        enum_t.fields.push((
            "choices".into(),
            PvField::ScalarArray(vec![
                ScalarValue::String("Off".into()),
                ScalarValue::String("On".into()),
                ScalarValue::String("Reset".into()),
            ]),
        ));
        let mut nt = PvStructure::new("epics:nt/NTEnum:1.0");
        nt.fields.push(("value".into(), PvField::Structure(enum_t)));
        // The carrier keeps BOTH the index and the choice labels so the
        // consumer (`convert_to`) can copy the label on a DBR_STRING target
        // (`pvxs/ioc/pvalink_lset.cpp:331-363`, `case DBR_STRING`) while every
        // numeric target still gets the index.
        assert_eq!(
            pvfield_to_epics_value(&PvField::Structure(nt)),
            Some(EpicsValue::EnumWithChoices {
                index: 2,
                choices: vec!["Off".into(), "On".into(), "Reset".into()],
            })
        );
    }

    #[test]
    fn pvfield_non_enum_struct_without_value_is_none() {
        use epics_pva_rs::pvdata::PvStructure;
        // A non-`enum_t` struct lacking a `value` child still returns
        // `None` — the enum_t branch must not change the fallback.
        let mut s = PvStructure::new("epics:nt/NTTable:1.0");
        s.fields
            .push(("labels".into(), PvField::Scalar(ScalarValue::Int(1))));
        assert_eq!(pvfield_to_epics_value(&PvField::Structure(s)), None);
    }

    /// An NTNDArray `value` is a discriminated union; conversion must
    /// follow it to its active member (pvxs `value.lookup("->")`,
    /// `pvxs/ioc/pvalink_lset.cpp:278-279`). Pre-fix a `PvField::Union` hit the
    /// catch-all and returned `None`, so an INP pvalink on an NTNDArray
    /// reported no usable value.
    #[test]
    fn pvfield_ntndarray_union_value_converts_to_active_member() {
        use epics_pva_rs::pvdata::PvStructure;
        let union = PvField::Union {
            selector: 9,
            variant_name: "floatValue".into(),
            value: Box::new(PvField::scalar_array_float(vec![1.5f32, 2.5, 3.5])),
        };
        // Directly on the selected union (the value-read path hands the
        // already-selected field here).
        assert_eq!(
            pvfield_to_epics_value(&union),
            Some(EpicsValue::FloatArray(vec![1.5, 2.5, 3.5].into())),
            "selected union must convert to its floatValue member"
        );
        // And on the whole NTNDArray struct: the `value`-child recursion
        // dereferences the union too.
        let mut nd = PvStructure::new("epics:nt/NTNDArray:1.0");
        nd.fields.push(("value".into(), union));
        assert_eq!(
            pvfield_to_epics_value(&PvField::Structure(nd)),
            Some(EpicsValue::FloatArray(vec![1.5, 2.5, 3.5].into())),
        );
    }

    /// string / float / short / char / typed-array shapes the
    /// previous best-effort converter dropped now round-trip through
    /// `EpicsValue`. The pvData `pvByte` (signed 8-bit) widens to
    /// `ShortArray` to preserve the negative range.
    #[test]
    fn pvfield_array_conversions_cover_pvxs_shapes() {
        use epics_pva_rs::pvdata::TypedScalarArray;

        // Untyped (Vec<ScalarValue>) variants.
        assert_eq!(
            pvfield_to_epics_value(&PvField::ScalarArray(vec![
                ScalarValue::Float(1.5),
                ScalarValue::Float(-2.5),
            ])),
            Some(EpicsValue::FloatArray(vec![1.5, -2.5].into()))
        );
        assert_eq!(
            pvfield_to_epics_value(&PvField::ScalarArray(vec![
                ScalarValue::Short(-7),
                ScalarValue::Short(8),
            ])),
            Some(EpicsValue::ShortArray(vec![-7, 8].into()))
        );
        assert_eq!(
            pvfield_to_epics_value(&PvField::ScalarArray(vec![
                ScalarValue::UByte(0x55),
                ScalarValue::UByte(0xFF),
            ])),
            Some(EpicsValue::CharArray(vec![0x55, 0xFF].into()))
        );
        // pvByte → ShortArray (signed widen).
        assert_eq!(
            pvfield_to_epics_value(&PvField::ScalarArray(vec![
                ScalarValue::Byte(-1),
                ScalarValue::Byte(2),
            ])),
            Some(EpicsValue::ShortArray(vec![-1, 2].into()))
        );
        assert_eq!(
            pvfield_to_epics_value(&PvField::ScalarArray(vec![
                ScalarValue::String("a".into()),
                ScalarValue::String("b".into()),
            ])),
            Some(EpicsValue::StringArray(vec!["a".into(), "b".into()].into()))
        );

        // Typed-fast-path variants emitted by the wire decoder.
        assert_eq!(
            pvfield_to_epics_value(&PvField::ScalarArrayTyped(TypedScalarArray::Float(
                vec![3.25f32, -4.5].into()
            ))),
            Some(EpicsValue::FloatArray(vec![3.25, -4.5].into()))
        );
        assert_eq!(
            pvfield_to_epics_value(&PvField::ScalarArrayTyped(TypedScalarArray::String(
                vec!["x".into(), "y".into()].into()
            ))),
            Some(EpicsValue::StringArray(vec!["x".into(), "y".into()].into()))
        );
        assert_eq!(
            pvfield_to_epics_value(&PvField::ScalarArrayTyped(TypedScalarArray::UByte(
                vec![1u8, 2, 3].into()
            ))),
            Some(EpicsValue::CharArray(vec![1, 2, 3].into()))
        );
    }

    /// a remote PVA `long` / `ulong` scalar or array carries
    /// 64 bits. The pvalink INP conversion previously collapsed it to
    /// `EpicsValue::Long` / `LongArray` (i32), dropping the upper 32
    /// bits before the local database could coerce the value. It must
    /// now preserve the full width as `Int64` / `UInt64` /
    /// `Int64Array` / `UInt64Array` — the same typed-conversion
    /// contract QSRV uses.
    #[test]
    fn ex_r8_inp_long_ulong_preserve_full_width() {
        use epics_pva_rs::pvdata::TypedScalarArray;

        // A `ulong` value whose upper 32 bits are non-zero — i32
        // truncation would lose it entirely.
        let big_u: u64 = 0x1234_5678_9ABC_DEF0;
        let big_i: i64 = -0x0123_4567_89AB_CDEF;

        // Scalar `ulong` → `UInt64` (full width).
        assert_eq!(
            pvfield_to_epics_value(&PvField::Scalar(ScalarValue::ULong(big_u))),
            Some(EpicsValue::UInt64(big_u)),
            "remote ulong scalar must keep all 64 bits"
        );
        // Scalar `long` → `Int64` (full width).
        assert_eq!(
            pvfield_to_epics_value(&PvField::Scalar(ScalarValue::Long(big_i))),
            Some(EpicsValue::Int64(big_i)),
            "remote long scalar must keep all 64 bits"
        );

        // Untyped `ulong[]` → `UInt64Array`.
        assert_eq!(
            pvfield_to_epics_value(&PvField::ScalarArray(vec![
                ScalarValue::ULong(big_u),
                ScalarValue::ULong(1),
            ])),
            Some(EpicsValue::UInt64Array(vec![big_u, 1].into())),
            "remote ulong[] must keep all 64 bits per element"
        );
        // Untyped `long[]` → `Int64Array`.
        assert_eq!(
            pvfield_to_epics_value(&PvField::ScalarArray(vec![
                ScalarValue::Long(big_i),
                ScalarValue::Long(-1),
            ])),
            Some(EpicsValue::Int64Array(vec![big_i, -1].into())),
            "remote long[] must keep all 64 bits per element"
        );

        // Typed-fast-path `ulong[]` → `UInt64Array`.
        assert_eq!(
            pvfield_to_epics_value(&PvField::ScalarArrayTyped(TypedScalarArray::ULong(
                vec![big_u, 2].into()
            ))),
            Some(EpicsValue::UInt64Array(vec![big_u, 2].into())),
            "typed remote ulong[] must keep all 64 bits per element"
        );
        // Typed-fast-path `long[]` → `Int64Array`.
        assert_eq!(
            pvfield_to_epics_value(&PvField::ScalarArrayTyped(TypedScalarArray::Long(
                vec![big_i, -2].into()
            ))),
            Some(EpicsValue::Int64Array(vec![big_i, -2].into())),
            "typed remote long[] must keep all 64 bits per element"
        );
    }

    /// A remote PVA `uint` / `uint[]` is unsigned 32-bit, and the link
    /// advertises DBF_ULONG metadata. The INP conversion previously
    /// carried it as `EpicsValue::Long` / `LongArray` (i32), sign-casting
    /// any value above i32::MAX to a negative number — contradicting the
    /// unsigned metadata. It must now widen losslessly to `Int64` /
    /// `Int64Array` (pvxs maps `TypeCode::UInt32 => DBF_ULONG` and routes
    /// the arrays through `ArrayType::UInt32`,
    /// `pvxs/ioc/pvalink_lset.cpp:222`, `:287-328`).
    #[test]
    fn inp_uint_preserves_unsigned_value_above_i32_max() {
        use epics_pva_rs::pvdata::TypedScalarArray;

        // 0x8000_0000 = 2147483648: the smallest u32 that overflows i32.
        let big: u32 = 0x8000_0000;
        let big_i: i64 = 2_147_483_648;

        // Scalar `uint` → `Int64` (positive, not the sign-cast -2147483648).
        assert_eq!(
            pvfield_to_epics_value(&PvField::Scalar(ScalarValue::UInt(big))),
            Some(EpicsValue::Int64(big_i)),
            "remote uint scalar above i32::MAX must stay unsigned, not sign-cast negative"
        );

        // Untyped `uint[]` → `Int64Array`.
        assert_eq!(
            pvfield_to_epics_value(&PvField::ScalarArray(vec![
                ScalarValue::UInt(big),
                ScalarValue::UInt(1),
            ])),
            Some(EpicsValue::Int64Array(vec![big_i, 1].into())),
            "remote uint[] above i32::MAX must stay unsigned per element"
        );

        // Typed-fast-path `uint[]` → `Int64Array`.
        assert_eq!(
            pvfield_to_epics_value(&PvField::ScalarArrayTyped(TypedScalarArray::UInt(
                vec![big, 2].into()
            ))),
            Some(EpicsValue::Int64Array(vec![big_i, 2].into())),
            "typed remote uint[] above i32::MAX must stay unsigned per element"
        );
    }

    // ---- B3: monitor-notification forwarder wiring ----

    use crate::pvalink::config::SevrMode;
    use epics_pva_rs::pvdata::PvStructure;

    fn nt_scalar(v: f64) -> PvField {
        let mut s = PvStructure::new("epics:nt/NTScalar:1.0");
        s.fields
            .push(("value".into(), PvField::Scalar(ScalarValue::Double(v))));
        PvField::Structure(s)
    }

    /// The default-variant [`MonitorKey`] for `pv` — the identity a
    /// bare / default-`Q` INP link registers under (matches the key
    /// `open_link_for_record` derives via `MonitorKey::from_config`).
    fn mk(pv: &str) -> MonitorKey {
        MonitorKey::from_config(&default_inp_cfg(pv))
    }

    /// A minimal record whose `process()` bumps a shared counter, so
    /// a test can observe how many times the B3 forwarder processed
    /// it.
    struct CountingRecord {
        count: Arc<std::sync::atomic::AtomicU32>,
    }

    impl epics_base_rs::server::record::Record for CountingRecord {
        fn record_type(&self) -> &'static str {
            "ai"
        }
        fn process(
            &mut self,
        ) -> epics_base_rs::error::CaResult<epics_base_rs::server::record::ProcessOutcome> {
            self.count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(epics_base_rs::server::record::ProcessOutcome::complete())
        }
        fn get_field(&self, name: &str) -> Option<EpicsValue> {
            (name == "VAL").then_some(EpicsValue::Double(0.0))
        }
        fn put_field(
            &mut self,
            name: &str,
            _value: EpicsValue,
        ) -> epics_base_rs::error::CaResult<()> {
            Err(epics_base_rs::error::CaError::FieldNotFound(name.into()))
        }
        fn declared_fields(&self) -> &'static [epics_base_rs::server::record::FieldDesc] {
            &[]
        }
    }

    /// B3: a monitor event delivered on the forwarder channel
    /// processes the registered owning record.
    #[tokio::test]
    async fn b3_forwarder_processes_owning_record_on_update() {
        let db = PvDatabase::new();
        let count = Arc::new(std::sync::atomic::AtomicU32::new(0));
        db.add_record(
            "DEST",
            Box::new(CountingRecord {
                count: count.clone(),
            }),
        )
        .await
        .unwrap();

        // CP target: scans on every event.
        let mut fanout = ScanFanout::default();
        fanout.records.push(ScanTarget {
            field: "INP".into(),
            record: "DEST".into(),
            monorder: 0,
            atomic: false,
            passive_only: false,
        });
        let scan_targets: ScanTargetMap =
            Arc::new(parking_lot::RwLock::new(std::collections::HashMap::from([
                (mk("SRC"), fanout),
            ])));
        let db_slot = Arc::new(parking_lot::RwLock::new(Some(db)));

        let (tx, rx) = tokio::sync::mpsc::channel::<ScanEvent>(8);
        let forwarder = tokio::spawn(run_notify_forwarder(
            mk("SRC"),
            rx,
            scan_targets,
            db_slot,
            Arc::new(ScanOverrun::default()),
        ));

        // Two distinct values → two scans.
        tx.send(ScanEvent::Value(nt_scalar(1.0))).await.unwrap();
        tx.send(ScanEvent::Value(nt_scalar(2.0))).await.unwrap();
        drop(tx); // close channel so the forwarder loop ends
        forwarder.await.unwrap();

        assert_eq!(count.load(std::sync::atomic::Ordering::SeqCst), 2);
    }

    /// PACT pre-check: a CP scan onto a target already processing async
    /// (PACT) must set `rpro = true` and NOT re-process it, mirroring
    /// pvxs `ScanTrack::scan` (`pvxs/ioc/pvalink_channel.cpp:313-325`,
    /// `else if (prec->pact) { prec->rpro = TRUE; }`). Pre-fix the scan
    /// drove the PACT target through the `dbProcess` entry guard, which
    /// bumps LCNT toward SCAN_ALARM and never sets RPRO — so a fast
    /// monitor stream onto a stuck async target eventually alarmed.
    #[tokio::test]
    async fn scan_sets_rpro_and_skips_pact_target() {
        let db = PvDatabase::new();
        let count = Arc::new(std::sync::atomic::AtomicU32::new(0));
        db.add_record(
            "DEST",
            Box::new(CountingRecord {
                count: count.clone(),
            }),
        )
        .await
        .unwrap();

        // Hold the record Arc and drive it into PACT (async in
        // progress) before the db moves into the forwarder slot.
        let dest = db.get_record("DEST").unwrap();
        dest.write().enter_pact();

        // CP target: scans on every event.
        let mut fanout = ScanFanout::default();
        fanout.records.push(ScanTarget {
            field: "INP".into(),
            record: "DEST".into(),
            monorder: 0,
            atomic: false,
            passive_only: false,
        });
        let scan_targets: ScanTargetMap =
            Arc::new(parking_lot::RwLock::new(std::collections::HashMap::from([
                (mk("SRC"), fanout),
            ])));
        let db_slot = Arc::new(parking_lot::RwLock::new(Some(db)));

        let (tx, rx) = tokio::sync::mpsc::channel::<ScanEvent>(8);
        let forwarder = tokio::spawn(run_notify_forwarder(
            mk("SRC"),
            rx,
            scan_targets,
            db_slot,
            Arc::new(ScanOverrun::default()),
        ));

        tx.send(ScanEvent::Value(nt_scalar(1.0))).await.unwrap();
        drop(tx);
        forwarder.await.unwrap();

        // The async target was NOT re-processed by the CP scan ...
        assert_eq!(
            count.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "a PACT target must not be re-processed by a CP scan"
        );
        // ... and RPRO was armed so the standard RPRO mechanism
        // reprocesses it once the async cycle completes.
        assert!(
            dest.read().common.rpro != 0,
            "a PACT target must get rpro=true (pvxs prec->rpro = TRUE)"
        );
    }

    /// pvxs `ScanTrack::scan` runs with the record locked
    /// (`pvalink_channel.cpp:312`), so a record cannot go active between the
    /// PACT test and the process it gates. Another thread that takes the set
    /// whenever this scan lets it go, and leaves the record active, must find
    /// no window: the scan processes it once, and it never meets
    /// `dbProcess`'s PACT guard (LCNT).
    #[tokio::test]
    async fn non_atomic_scan_tests_pact_and_processes_under_one_hold() {
        let db = PvDatabase::new();
        let count = Arc::new(std::sync::atomic::AtomicU32::new(0));
        db.add_record(
            "DEST",
            Box::new(CountingRecord {
                count: count.clone(),
            }),
        )
        .await
        .unwrap();
        let dest = db.get_record("DEST").unwrap();
        // Off the bootstrap set onto its own, as `iocInit` leaves it, so
        // every hold below is of the one set.
        drop(db.lock_records(["DEST"]));

        let mut fanout = ScanFanout::default();
        fanout.records.push(ScanTarget {
            field: "INP".into(),
            record: "DEST".into(),
            monorder: 0,
            atomic: false,
            passive_only: false,
        });
        let scan_targets: ScanTargetMap =
            Arc::new(parking_lot::RwLock::new(std::collections::HashMap::from([
                (mk("SRC"), fanout),
            ])));
        let db_slot = Arc::new(parking_lot::RwLock::new(Some(db)));

        // The first hold is the scan's own; any later one comes after the set
        // was free, where the other thread goes.
        let holds = std::rc::Rc::new(std::cell::Cell::new(0));
        let armed = {
            let (holds, dest) = (holds.clone(), dest.clone());
            epics_base_rs::server::database::fresh_hold_hook::arm(dest.clone(), move || {
                holds.set(holds.get() + 1);
                if holds.get() == 2 {
                    dest.write().enter_pact();
                }
            })
        };
        scan_once(&mk("SRC"), &scan_targets, &db_slot).await;
        drop(armed);

        assert_eq!(
            dest.read().common.lcnt,
            0,
            "the scan met dbProcess's PACT guard"
        );
        assert_eq!(count.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert_eq!(
            holds.get(),
            1,
            "DEST's set was free between test and process"
        );
    }

    /// Attach-time scan (`pvxs/ioc/pvalink_lset.cpp:148-167`): attaching a
    /// record to an ALREADY-connected, reused monitor variant fires one
    /// immediate scan, so the record does not stay unprocessed until the
    /// next upstream update. Reuse is simulated by seeding the registry
    /// with a connected link and marking its forwarder already started
    /// (so `spawn_notify_forwarder` takes the early-return reuse path).
    /// Pre-fix no attach scan fired and the record processed zero times.
    #[tokio::test]
    async fn attach_time_scan_fires_on_reused_connected_monitor() {
        let db = PvDatabase::new();
        let count = Arc::new(std::sync::atomic::AtomicU32::new(0));
        db.add_record(
            "DEST",
            Box::new(CountingRecord {
                count: count.clone(),
            }),
        )
        .await
        .unwrap();

        let resolver = PvaLinkResolver::new(crate::test_reactor());
        resolver.attach_database(db);

        // A connected, already-open CP monitor variant for SRC.
        let cfg = PvaLinkConfig {
            monitor: true,
            scan_on_update: true,
            ..PvaLinkConfig::defaults_for("SRC", LinkDirection::Inp)
        };
        let (link, connected) = PvaLink::for_test_with_monitor_flag(cfg.clone(), None);
        connected.store(true, std::sync::atomic::Ordering::Release);
        resolver.registry.insert_for_test(&cfg, Arc::new(link));
        // Mark the forwarder already started → reuse path (no fresh
        // first-event scan is wired for the newly attached record).
        resolver
            .forwarders
            .lock()
            .insert(MonitorKey::from_config(&cfg));

        resolver
            .open_inp_cfg(cfg, "SRC".to_string(), Some(inp("DEST").into()))
            .await
            .unwrap();

        // Queued, as C `scanOnce`.
        for _ in 0..200 {
            if count.load(std::sync::atomic::Ordering::SeqCst) != 0 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert_eq!(
            count.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "attaching to a connected reused monitor must fire one immediate scan"
        );
    }

    /// The attach-time scan is gated on `is_connected()`: attaching to a
    /// monitor variant that has NOT yet connected fires no scan (the
    /// record's first scan comes from `on_event` once the monitor
    /// connects), matching pvxs's `if(self->lchan->connected)` guard.
    #[tokio::test]
    async fn attach_time_scan_skipped_when_not_connected() {
        let db = PvDatabase::new();
        let count = Arc::new(std::sync::atomic::AtomicU32::new(0));
        db.add_record(
            "DEST",
            Box::new(CountingRecord {
                count: count.clone(),
            }),
        )
        .await
        .unwrap();

        let resolver = PvaLinkResolver::new(crate::test_reactor());
        resolver.attach_database(db);

        let cfg = PvaLinkConfig {
            monitor: true,
            scan_on_update: true,
            ..PvaLinkConfig::defaults_for("SRC", LinkDirection::Inp)
        };
        // Connection flag left FALSE → not connected at attach.
        let (link, _connected) = PvaLink::for_test_with_monitor_flag(cfg.clone(), None);
        resolver.registry.insert_for_test(&cfg, Arc::new(link));
        resolver
            .forwarders
            .lock()
            .insert(MonitorKey::from_config(&cfg));

        resolver
            .open_inp_cfg(cfg, "SRC".to_string(), Some(inp("DEST").into()))
            .await
            .unwrap();

        // Long enough for a queued scan to have run.
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        assert_eq!(
            count.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "a not-yet-connected monitor must not fire an attach-time scan"
        );
    }

    /// Scan-trigger overrun, consumer half: with `Q=1` and a saturated
    /// scan-trigger queue, two monitor events must NOT collapse into one
    /// silent missing CP/CPP process. The first event sits in the full
    /// `Q=1` channel; the second overran and coalesced into an overrun
    /// marker (`ScanOverrun::mark`, as `enqueue_scan_trigger` does on a
    /// full queue). The forwarder drains BOTH — the queued event and the
    /// coalesced owed scan — so the owning record processes twice, with
    /// the overrun explicitly counted, never silently dropped.
    #[tokio::test]
    async fn br69_full_queue_overrun_still_scans_no_silent_loss() {
        let db = PvDatabase::new();
        let count = Arc::new(std::sync::atomic::AtomicU32::new(0));
        db.add_record(
            "DEST",
            Box::new(CountingRecord {
                count: count.clone(),
            }),
        )
        .await
        .unwrap();

        let mut fanout = ScanFanout::default();
        fanout.records.push(ScanTarget {
            field: "INP".into(),
            record: "DEST".into(),
            monorder: 0,
            atomic: false,
            passive_only: false,
        });
        let scan_targets: ScanTargetMap =
            Arc::new(parking_lot::RwLock::new(std::collections::HashMap::from([
                (mk("SRC"), fanout),
            ])));
        let db_slot = Arc::new(parking_lot::RwLock::new(Some(db)));

        // Q=1: the queue holds exactly one trigger.
        let (tx, rx) = tokio::sync::mpsc::channel::<ScanEvent>(1);
        let overrun = Arc::new(ScanOverrun::default());

        // First monitor event fills the Q=1 queue.
        tx.try_send(ScanEvent::Value(nt_scalar(1.0))).unwrap();
        // Second monitor event finds the queue full → coalesces to the
        // latest cache + overrun marker instead of a silent drop. This is
        // exactly what `enqueue_scan_trigger` does on a full queue.
        overrun.mark();

        let forwarder = tokio::spawn(run_notify_forwarder(
            mk("SRC"),
            rx,
            scan_targets,
            db_slot,
            overrun.clone(),
        ));
        drop(tx); // close so the forwarder loop ends after draining
        forwarder.await.unwrap();

        // Two events delivered to the callback → two record processes
        // (one queued, one coalesced), NOT one silent miss.
        assert_eq!(
            count.load(std::sync::atomic::Ordering::SeqCst),
            2,
            "the overrun-coalesced event must still drive a CP scan"
        );
        assert_eq!(
            overrun.count(),
            1,
            "the overrun is explicitly counted, not silently dropped"
        );
    }

    /// Repeated *identical* monitor values still
    /// process a CP target on EVERY event. pvxs has no value-difference
    /// gate and ignores the `always` option at scan time
    /// (`pvxs/documentation/pvalink.rst:102`,
    /// `pvxs/ioc/pvalink_channel.cpp:389-431`), so event-driven side
    /// effects (timestamps, counters, FLNK chains) fire on every post.
    /// Pre-fix the forwarder suppressed no-op updates unless `always`,
    /// diverging from upstream.
    #[tokio::test]
    async fn br51_repeated_identical_values_still_scan() {
        let db = PvDatabase::new();
        let count = Arc::new(std::sync::atomic::AtomicU32::new(0));
        db.add_record(
            "DEST",
            Box::new(CountingRecord {
                count: count.clone(),
            }),
        )
        .await
        .unwrap();

        let mut fanout = ScanFanout::default();
        fanout.records.push(ScanTarget {
            field: "INP".into(),
            record: "DEST".into(),
            monorder: 0,
            atomic: false,
            passive_only: false,
        });
        let scan_targets: ScanTargetMap =
            Arc::new(parking_lot::RwLock::new(std::collections::HashMap::from([
                (mk("SRC"), fanout),
            ])));
        let db_slot = Arc::new(parking_lot::RwLock::new(Some(db)));

        let (tx, rx) = tokio::sync::mpsc::channel::<ScanEvent>(8);
        let forwarder = tokio::spawn(run_notify_forwarder(
            mk("SRC"),
            rx,
            scan_targets,
            db_slot,
            Arc::new(ScanOverrun::default()),
        ));

        // Three identical values — pvxs processes all three.
        tx.send(ScanEvent::Value(nt_scalar(1.0))).await.unwrap();
        tx.send(ScanEvent::Value(nt_scalar(1.0))).await.unwrap();
        tx.send(ScanEvent::Value(nt_scalar(1.0))).await.unwrap();
        drop(tx);
        forwarder.await.unwrap();

        assert_eq!(
            count.load(std::sync::atomic::Ordering::SeqCst),
            3,
            "every identical monitor event must still scan (no no-op suppression)"
        );
    }

    /// A `Disconnected` lifecycle event scans CP and passive-CPP
    /// targets even though no value accompanies it, so the owning
    /// record processes the upstream disconnect (and can expose
    /// LINK_ALARM/INVALID). pvxs runs the same scan loop after the
    /// `catch(client::Disconnect&)` branch as after a value update
    /// (`pvxs/ioc/pvalink_channel.cpp:360-373` + `:422-432`).
    ///
    /// Pre-fix the forwarder channel carried only `PvField` values, so
    /// a disconnect produced no event and the record was never
    /// processed until some later unrelated trigger — here that would
    /// leave each count at 1 (the single value) instead of 2.
    #[tokio::test]
    async fn forwarder_scans_cp_and_passive_cpp_on_disconnect() {
        let db = PvDatabase::new();
        let cp_count = Arc::new(std::sync::atomic::AtomicU32::new(0));
        let cpp_count = Arc::new(std::sync::atomic::AtomicU32::new(0));
        db.add_record(
            "CP_REC",
            Box::new(CountingRecord {
                count: cp_count.clone(),
            }),
        )
        .await
        .unwrap();
        // CPP target's owner defaults to SCAN=Passive, so the
        // `passive_only` gate admits it.
        db.add_record(
            "CPP_REC",
            Box::new(CountingRecord {
                count: cpp_count.clone(),
            }),
        )
        .await
        .unwrap();

        let mut fanout = ScanFanout::default();
        // CP: scans on every event, not passive-restricted.
        fanout.records.push(ScanTarget {
            field: "INP".into(),
            record: "CP_REC".into(),
            monorder: 0,
            atomic: false,
            passive_only: false,
        });
        // CPP: passive-restricted; owner is Passive so it is eligible.
        fanout.records.push(ScanTarget {
            field: "INP".into(),
            record: "CPP_REC".into(),
            monorder: 1,
            atomic: false,
            passive_only: true,
        });
        let scan_targets: ScanTargetMap =
            Arc::new(parking_lot::RwLock::new(std::collections::HashMap::from([
                (mk("SRC"), fanout),
            ])));
        let db_slot = Arc::new(parking_lot::RwLock::new(Some(db)));

        let (tx, rx) = tokio::sync::mpsc::channel::<ScanEvent>(8);
        let forwarder = tokio::spawn(run_notify_forwarder(
            mk("SRC"),
            rx,
            scan_targets,
            db_slot,
            Arc::new(ScanOverrun::default()),
        ));

        // One value (each target scans once), then a disconnect with no
        // trailing value (each target must scan again).
        tx.send(ScanEvent::Value(nt_scalar(1.0))).await.unwrap();
        tx.send(ScanEvent::Disconnected).await.unwrap();
        drop(tx);
        forwarder.await.unwrap();

        assert_eq!(
            cp_count.load(std::sync::atomic::Ordering::SeqCst),
            2,
            "CP target must process on the value AND the disconnect"
        );
        assert_eq!(
            cpp_count.load(std::sync::atomic::Ordering::SeqCst),
            2,
            "passive CPP target must process on the value AND the disconnect"
        );
    }

    /// B3: a pvalink-driven update fans out through the owning
    /// record's FLNK chain. The forwarder must call
    /// `process_record_with_links` (not the bare `process_record`),
    /// otherwise a CP pvalink feeding a calc record never propagates
    /// to the calc's FLNK target.
    #[tokio::test]
    async fn b3_forwarder_propagates_flnk_chain() {
        let db = PvDatabase::new();
        // DEST is the pvalink target; DOWNSTREAM is DEST's FLNK.
        let dest_count = Arc::new(std::sync::atomic::AtomicU32::new(0));
        let down_count = Arc::new(std::sync::atomic::AtomicU32::new(0));
        db.add_record(
            "DEST",
            Box::new(CountingRecord {
                count: dest_count.clone(),
            }),
        )
        .await
        .unwrap();
        db.add_record(
            "DOWNSTREAM",
            Box::new(CountingRecord {
                count: down_count.clone(),
            }),
        )
        .await
        .unwrap();
        // Wire DEST.FLNK -> DOWNSTREAM.
        db.put_pv_no_process("DEST.FLNK", EpicsValue::String("DOWNSTREAM".into()))
            .await
            .expect("set FLNK");

        // The forward links bind at iocInit, as C's do.
        db.ioc_init().await;

        let mut fanout = ScanFanout::default();
        fanout.records.push(ScanTarget {
            field: "INP".into(),
            record: "DEST".into(),
            monorder: 0,
            atomic: false,
            passive_only: false,
        });
        let scan_targets: ScanTargetMap =
            Arc::new(parking_lot::RwLock::new(std::collections::HashMap::from([
                (mk("SRC"), fanout),
            ])));
        let db_slot = Arc::new(parking_lot::RwLock::new(Some(db)));

        let (tx, rx) = tokio::sync::mpsc::channel::<ScanEvent>(8);
        let forwarder = tokio::spawn(run_notify_forwarder(
            mk("SRC"),
            rx,
            scan_targets,
            db_slot,
            Arc::new(ScanOverrun::default()),
        ));

        tx.send(ScanEvent::Value(nt_scalar(5.0))).await.unwrap();
        drop(tx);
        forwarder.await.unwrap();

        // DEST processed once, and its FLNK fanned out to DOWNSTREAM.
        assert_eq!(dest_count.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert_eq!(
            down_count.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "FLNK target must process via process_record_with_links"
        );
    }

    /// B3: `open_link_for_record` registers a `proc=CP` link's scan
    /// target and retains its parsed options (B2 `sevr` included).
    #[tokio::test]
    async fn b3_open_link_for_record_registers_scan_target() {
        let resolver = PvaLinkResolver::new(crate::test_reactor());
        // proc=CP → scan_on_update; sevr=MS exercised together.
        let _ = resolver
            .open_link_for_record("pva://SRC:PV?proc=CP&sevr=MS", inp("MY:REC"))
            .await;
        // Scan target registered under the bare PV name.
        let targets = resolver.scan_targets.read();
        let fanout = targets.get(&mk("SRC:PV")).expect("scan target registered");
        assert_eq!(fanout.records.len(), 1);
        assert_eq!(fanout.records[0].record, "MY:REC");
        drop(targets);
        // options retained under the full query-bearing key.
        let opts = resolver.link_options.read();
        let cfg = opts
            .get("SRC:PV?proc=CP&sevr=MS")
            .expect("link options retained");
        assert_eq!(cfg.sevr, SevrMode::Ms);
        assert!(cfg.scan_on_update);
    }

    /// B3: a non-CP link (`proc=NPP`) opened with a record does NOT
    /// register a scan target — only CP/CPP fan out.
    #[tokio::test]
    async fn b3_non_cp_link_registers_no_scan_target() {
        let resolver = PvaLinkResolver::new(crate::test_reactor());
        let _ = resolver
            .open_link_for_record("pva://OTHER:PV?proc=NPP", inp("REC2"))
            .await;
        assert!(resolver.scan_targets.read().get(&mk("OTHER:PV")).is_none());
    }

    /// B2 through the resolver: `open_link` retains `sevr` so a later
    /// full-string `inp_cfg_for` query reflects the `MSI` mode.
    /// key is the full query-bearing string, not the bare PV name.
    #[tokio::test]
    async fn b2_open_link_retains_sevr_mode() {
        let resolver = PvaLinkResolver::new(crate::test_reactor());
        let _ = resolver.open_link("pva://A:PV?sevr=MSI").await;
        // look up by the full link string (with query) — that is the key.
        let cfg = resolver.inp_cfg_for("A:PV?sevr=MSI").clone();
        assert_eq!(cfg.sevr, SevrMode::Msi);
        // A PV never opened falls back to NMS default.
        assert_eq!(resolver.inp_cfg_for("UNSEEN").sevr, SevrMode::Nms);
    }

    /// `LinkSet::link_names()` must surface the opened INP upstream PV
    /// names (the lset enumeration contract), each landing on the right
    /// link when fed back through its `is_connected` companion query; OUT
    /// links are excluded (no monitor connection signal). These names are
    /// NOT consumed by the iocInit wait — that is CA-facility only and
    /// pvalink never blocks init (pvxs parity) — but the round-trip
    /// `link_names → is_connected` identity contract still holds.
    #[tokio::test]
    async fn fr15_link_names_reports_opened_inp_pvs_queryable_by_is_connected() {
        let resolver = PvaLinkResolver::new(crate::test_reactor());

        // A connected INP link (for_test seeds a cached value, so
        // is_connected() reads true), a pending INP link (no value yet),
        // and an OUT link (must be excluded).
        let connected_cfg = resolver.inp_cfg_for("FR15:CONNECTED");
        let pending_cfg = resolver.inp_cfg_for("FR15:PENDING");
        let out_cfg = resolver.out_cfg_for("FR15:OUT");
        resolver.registry.insert_for_test(
            &connected_cfg,
            Arc::new(PvaLink::for_test(
                PvaLinkConfig::clone(&connected_cfg),
                Some(PvField::Scalar(ScalarValue::Double(1.0))),
            )),
        );
        resolver.registry.insert_for_test(
            &pending_cfg,
            Arc::new(PvaLink::for_test(PvaLinkConfig::clone(&pending_cfg), None)),
        );
        resolver.registry.insert_for_test(
            &out_cfg,
            Arc::new(PvaLink::for_test(PvaLinkConfig::clone(&out_cfg), None)),
        );

        let mut names = LinkSet::link_names(&resolver);
        names.sort();
        assert_eq!(
            names,
            vec!["FR15:CONNECTED".to_string(), "FR15:PENDING".to_string()],
            "opened INP PV names; OUT excluded"
        );

        // Each returned name is queryable via is_connected — the
        // round-trip identity the enumeration contract guarantees.
        assert!(
            LinkSet::is_connected(&resolver, b"FR15:CONNECTED"),
            "cached INP link reports connected"
        );
        assert!(
            !LinkSet::is_connected(&resolver, b"FR15:PENDING"),
            "INP link with no value yet reports not connected"
        );
    }

    // ---- B4: local / atomic / monorder forwarder effects ----

    /// A record that appends its name to a shared log on `process()`,
    /// so a test can observe the *order* in which the B4 forwarder
    /// scans records.
    struct OrderRecord {
        name: &'static str,
        log: Arc<parking_lot::Mutex<Vec<&'static str>>>,
    }

    impl epics_base_rs::server::record::Record for OrderRecord {
        fn record_type(&self) -> &'static str {
            "ai"
        }
        fn process(
            &mut self,
        ) -> epics_base_rs::error::CaResult<epics_base_rs::server::record::ProcessOutcome> {
            self.log.lock().push(self.name);
            Ok(epics_base_rs::server::record::ProcessOutcome::complete())
        }
        fn get_field(&self, _n: &str) -> Option<EpicsValue> {
            Some(EpicsValue::Double(0.0))
        }
        fn put_field(&mut self, _n: &str, _v: EpicsValue) -> epics_base_rs::error::CaResult<()> {
            Ok(())
        }
        fn declared_fields(&self) -> &'static [epics_base_rs::server::record::FieldDesc] {
            &[]
        }
    }

    /// B4 `monorder` + `atomic`: the forwarder scans the atomic group
    /// first, and `monorder` (low → high) within each group.
    #[tokio::test]
    async fn b4_forwarder_orders_by_atomic_then_monorder() {
        let db = PvDatabase::new();
        let log = Arc::new(parking_lot::Mutex::new(Vec::new()));
        for name in ["A", "B", "C", "D"] {
            db.add_record(
                name,
                Box::new(OrderRecord {
                    name,
                    log: log.clone(),
                }),
            )
            .await
            .unwrap();
        }

        let mut fanout = ScanFanout::default();
        // Non-atomic, monorder 1 / -1.
        fanout.records.push(ScanTarget {
            field: "INP".into(),
            record: "C".into(),
            monorder: 1,
            atomic: false,
            passive_only: false,
        });
        fanout.records.push(ScanTarget {
            field: "INP".into(),
            record: "D".into(),
            monorder: -1,
            atomic: false,
            passive_only: false,
        });
        // Atomic, monorder 5 / 0.
        fanout.records.push(ScanTarget {
            field: "INP".into(),
            record: "A".into(),
            monorder: 5,
            atomic: true,
            passive_only: false,
        });
        fanout.records.push(ScanTarget {
            field: "INP".into(),
            record: "B".into(),
            monorder: 0,
            atomic: true,
            passive_only: false,
        });
        let scan_targets: ScanTargetMap =
            Arc::new(parking_lot::RwLock::new(std::collections::HashMap::from([
                (mk("SRC"), fanout),
            ])));
        let db_slot = Arc::new(parking_lot::RwLock::new(Some(db)));

        let (tx, rx) = tokio::sync::mpsc::channel::<ScanEvent>(4);
        let forwarder = tokio::spawn(run_notify_forwarder(
            mk("SRC"),
            rx,
            scan_targets,
            db_slot,
            Arc::new(ScanOverrun::default()),
        ));
        tx.send(ScanEvent::Value(nt_scalar(1.0))).await.unwrap();
        drop(tx);
        forwarder.await.unwrap();

        // atomic group first (B by monorder 0, then A by 5), then
        // non-atomic (D by -1, then C by 1).
        assert_eq!(*log.lock(), vec!["B", "A", "D", "C"]);
    }

    /// B4 `atomic`: an atomic target scans on every monitor event,
    /// including a repeated identical value. After this fix,
    /// this is the same rule that governs every CP/CPP target (no no-op
    /// suppression); the test is retained to guard the atomic path
    /// specifically, since atomic targets run under the shared
    /// multi-record lock.
    #[tokio::test]
    async fn b4_atomic_scans_even_on_no_op_update() {
        let db = PvDatabase::new();
        let count = Arc::new(std::sync::atomic::AtomicU32::new(0));
        db.add_record(
            "DEST",
            Box::new(CountingRecord {
                count: count.clone(),
            }),
        )
        .await
        .unwrap();
        let mut fanout = ScanFanout::default();
        fanout.records.push(ScanTarget {
            field: "INP".into(),
            record: "DEST".into(),
            monorder: 0,
            atomic: true, // but atomic → scans anyway
            passive_only: false,
        });
        let scan_targets: ScanTargetMap =
            Arc::new(parking_lot::RwLock::new(std::collections::HashMap::from([
                (mk("SRC"), fanout),
            ])));
        let db_slot = Arc::new(parking_lot::RwLock::new(Some(db)));
        let (tx, rx) = tokio::sync::mpsc::channel::<ScanEvent>(4);
        let forwarder = tokio::spawn(run_notify_forwarder(
            mk("SRC"),
            rx,
            scan_targets,
            db_slot,
            Arc::new(ScanOverrun::default()),
        ));
        // Two identical values: both scan (pvxs does no value-diff).
        tx.send(ScanEvent::Value(nt_scalar(1.0))).await.unwrap();
        tx.send(ScanEvent::Value(nt_scalar(1.0))).await.unwrap();
        drop(tx);
        forwarder.await.unwrap();
        assert_eq!(count.load(std::sync::atomic::Ordering::SeqCst), 2);
    }

    /// B4 `local`: a `local`-flagged link whose PV is not a local
    /// record is rejected at open time.
    #[tokio::test]
    async fn b4_local_link_rejects_non_local_pv() {
        let db = Arc::new(PvDatabase::new());
        let resolver = install_pvalink_resolver(&db).await;
        let r = resolver
            .open_link("pva://NOT:A:LOCAL:RECORD?local=true")
            .await;
        let rejected = matches!(r, Err(PvaLinkError::NotLocal(_)));
        assert!(rejected, "local link to a non-local PV must be rejected");
    }

    /// B4 `local`: a non-local link to the same PV opens fine — the
    /// `local` gate only applies when the option is set.
    #[tokio::test]
    async fn b4_non_local_link_to_remote_pv_is_allowed() {
        let db = Arc::new(PvDatabase::new());
        let resolver = install_pvalink_resolver(&db).await;
        // No `local` option → open is not gated (it will just never
        // connect, which is fine for this assertion).
        let r = resolver.open_link("pva://SOME:REMOTE:PV").await;
        assert!(r.is_ok(), "non-local link should open");
    }

    /// The install scan opens a channel for a link that carries NO
    /// options, in both directions.
    ///
    /// pvxs `pvaOpenLink` (`pvxs/ioc/pvalink_lset.cpp:63-137`) creates and opens a
    /// `pvaLinkChannel` for every pvalink at link-init; the only early-out
    /// is an empty channel name. This scan used to skip any link whose
    /// verbatim string equalled its bare PV name, on the reasoning that
    /// there were no options to pre-register — which left the channel
    /// unopened. On the OUT side that is a dropped write, not just a
    /// missing diagnostic: `PvaLink::open` is what starts the
    /// connection-tracking monitor, and the non-retry write gate refuses a
    /// put while `is_connected()` is false, so the first put through a
    /// lazily-opened OUT link was always discarded.
    ///
    /// Measured on the RTEMS target (stage 5):
    /// `field(OUT,"{pva:{pv:'UPSTREAM:AO'}}")` — a pv-only longhand
    /// parses to a plain `Pva` — never appeared in `channel_diagnostics()`.
    #[tokio::test]
    async fn install_scan_opens_option_less_links_in_both_directions() {
        use epics_base_rs::server::records::ai::AiRecord;
        use epics_base_rs::server::records::ao::AoRecord;

        let db = Arc::new(PvDatabase::new());
        db.add_record("DOWN", Box::new(AiRecord::new(0.0)))
            .await
            .unwrap();
        db.add_record("UPLNK", Box::new(AoRecord::new(0.0)))
            .await
            .unwrap();
        db.get_record("DOWN").unwrap().write().common.text.inp = "pva://UP:AI".into();
        db.get_record("UPLNK").unwrap().write().common.text.out = "pva://UP:AO".into();

        let resolver = install_pvalink_resolver(&db).await;
        assert_eq!(
            resolver.link_count(),
            2,
            "both option-less links must be opened at install, got {:?}",
            resolver.channel_diagnostics()
        );
        let diags = resolver.channel_diagnostics();
        let inp = diags
            .iter()
            .find(|d| d.pv_name == "UP:AI")
            .unwrap_or_else(|| panic!("INP channel missing, got {diags:?}"));
        assert!(matches!(inp.direction, LinkDirection::Inp));
        assert_eq!(inp.records, [RecordName::from("DOWN")]);
        let out = diags
            .iter()
            .find(|d| d.pv_name == "UP:AO")
            .unwrap_or_else(|| panic!("OUT channel missing, got {diags:?}"));
        assert!(matches!(out.direction, LinkDirection::Out));
        assert_eq!(out.records, [RecordName::from("UPLNK")]);
    }

    /// B4 `local`: a `local`-flagged link to a non-record local PV —
    /// one registered via `add_pv` (e.g. an iocsh stats PV or QSRV
    /// single-record channel) — must NOT be rejected. The gate
    /// previously consulted `get_record` only and wrongly returned
    /// `NotLocal` for simple PVs.
    #[tokio::test]
    async fn b4_local_link_accepts_simple_pv() {
        let db = Arc::new(PvDatabase::new());
        db.add_pv("LOCAL:SIMPLE:PV", EpicsValue::Double(3.0))
            .await
            .unwrap();
        let resolver = install_pvalink_resolver(&db).await;
        let r = resolver.open_link("pva://LOCAL:SIMPLE:PV?local=true").await;
        assert!(
            r.is_ok(),
            "local link to a simple add_pv PV must be accepted"
        );
    }

    /// B4 `local` (dbChannelTest parity): a `local=true` link to a
    /// *field* of a local record is accepted — locality is channel-level,
    /// not record-name-only. Mirrors EPICS `dbChannelTest()` accepting
    /// `x`, `x.`, `x.VAL`, `x.NAME`, `x.INP`
    /// (`modules/database/test/ioc/db/dbChannelTest.c:175-179`); pvxs
    /// gates the option through that same channel parser
    /// (`ioc/pvalink_lset.cpp:69-75`).
    #[tokio::test]
    async fn b4_local_link_accepts_record_field() {
        use epics_base_rs::server::records::ai::AiRecord;
        let db = Arc::new(PvDatabase::new());
        db.add_record("REC", Box::new(AiRecord::new(1.0)))
            .await
            .unwrap();
        let resolver = install_pvalink_resolver(&db).await;
        for name in [
            "pva://REC?local=true",      // bare record (dbChannelTest "x")
            "pva://REC.?local=true",     // trailing dot → default field ("x.")
            "pva://REC.VAL?local=true",  // value field
            "pva://REC.NAME?local=true", // virtual field
            "pva://REC.DESC?local=true", // common field
            "pva://REC.INP?local=true",  // common link field
        ] {
            let r = resolver.open_link(name).await;
            assert!(
                r.is_ok(),
                "local link {name} to a local record field must be accepted, got {:?}",
                r.err()
            );
        }
    }

    /// B4 `local` (dbChannelTest parity): a `local=true` link to a
    /// *nonexistent* field of a local record is rejected — the field
    /// must resolve, matching `dbChannelTest("x.NOFIELD")` failing
    /// (`dbChannelTest.c:181`). A field on a nonexistent record is
    /// likewise rejected.
    #[tokio::test]
    async fn b4_local_link_rejects_nonexistent_field() {
        use epics_base_rs::server::records::ai::AiRecord;
        let db = Arc::new(PvDatabase::new());
        db.add_record("REC", Box::new(AiRecord::new(1.0)))
            .await
            .unwrap();
        let resolver = install_pvalink_resolver(&db).await;
        let bad_field = resolver.open_link("pva://REC.NOSUCH?local=true").await;
        assert!(
            matches!(bad_field, Err(PvaLinkError::NotLocal(_))),
            "local link to a nonexistent field must be rejected, got {:?}",
            bad_field.err()
        );
        let bad_record = resolver.open_link("pva://NOPE.VAL?local=true").await;
        assert!(
            matches!(bad_record, Err(PvaLinkError::NotLocal(_))),
            "local link to a nonexistent record must be rejected, got {:?}",
            bad_record.err()
        );
    }

    /// An OUT link with `local=true` to
    /// a PV not served by this IOC must be rejected at open time, exactly
    /// like the INP path. pvxs applies `pvaLinkConfig::local` inside
    /// `pvaOpenLink()` for every direction (`ioc/pvalink_lset.cpp:69-75`);
    /// before this fix the OUT open path stored the options and opened the
    /// remote channel without ever evaluating `local`.
    #[tokio::test]
    async fn b4_local_out_link_rejects_non_local_pv() {
        let db = Arc::new(PvDatabase::new());
        let resolver = install_pvalink_resolver(&db).await;
        let r = resolver
            .open_out_link("pva://NOT:A:LOCAL:RECORD?local=true", None)
            .await;
        assert!(
            matches!(r, Err(PvaLinkError::NotLocal(_))),
            "local OUT link to a non-local PV must be rejected, got {:?}",
            r.err()
        );
    }

    /// The structured pvxs-parity JSON
    /// OUT path (`open_json_out_link`) honours `local=true` the same way.
    #[tokio::test]
    async fn b4_local_json_out_link_rejects_non_local_pv() {
        let db = Arc::new(PvDatabase::new());
        let resolver = install_pvalink_resolver(&db).await;
        let r = resolver
            .open_json_out_link(
                "NOT:A:LOCAL:RECORD",
                // JSON boolean `local:true` — pvxs accepts `local` only as
                // a boolean (pva_parse_bool), so the kind matters here.
                &[("local".to_string(), JlinkValue::Bool(true))],
                None,
            )
            .await;
        assert!(
            matches!(r, Err(PvaLinkError::NotLocal(_))),
            "local JSON OUT link to a non-local PV must be rejected, got {:?}",
            r.err()
        );
    }

    /// The lazy write path
    /// (`LinkSet::put_value`, the hot path that opens an OUT link on first
    /// write) must reject a `local=true` write to a remote PV before
    /// opening/queuing, not send or stage a remote PUT. Asserts the error
    /// is the `local` gate (`NotLocal` Display) rather than an incidental
    /// disconnected-write failure, so removing the gate truly fails this.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn b4_local_out_put_value_rejects_non_local_pv() {
        let db = Arc::new(PvDatabase::new());
        let resolver = install_pvalink_resolver(&db).await;
        let r = LinkSet::put_value(
            &resolver,
            b"pva://NOT:A:LOCAL:RECORD?local=true",
            EpicsValue::Double(1.0),
            LinkPutOp::Plain,
        )
        .await;
        let err = r.expect_err("local=true OUT put to a non-local PV must fail");
        assert!(
            err.contains("has no matching local record"),
            "put must be rejected by the local gate, not an unrelated write failure: {err}"
        );
    }

    /// Sibling-cache: a non-local OUT
    /// link to a remote PV opens first and seeds the shared channel owner;
    /// a later `local=true` OUT link to the SAME PV must still be rejected
    /// rather than reusing the already-open remote owner. The gate runs
    /// before `get_or_open`, so a cache hit cannot bypass it.
    #[tokio::test]
    async fn b4_local_out_link_rejected_even_after_non_local_sibling_opened() {
        let db = Arc::new(PvDatabase::new());
        let resolver = install_pvalink_resolver(&db).await;
        // Non-local sibling opens fine and caches the shared owner.
        let opened = resolver.open_out_link("pva://SHARED:REMOTE:PV", None).await;
        assert!(
            opened.is_ok(),
            "non-local OUT link must open, got {:?}",
            opened.err()
        );
        // A later local=true link to the same PV must NOT reuse it.
        let gated = resolver
            .open_out_link("pva://SHARED:REMOTE:PV?local=true", None)
            .await;
        assert!(
            matches!(gated, Err(PvaLinkError::NotLocal(_))),
            "local=true OUT link must be rejected even when a non-local \
             sibling already opened the channel, got {:?}",
            gated.err()
        );
    }

    /// B4 `local` (dbChannelTest parity): field modifiers are accepted
    /// and ignored for the locality decision — a trailing `$` long-string
    /// modifier, an empty `{}` filter, and a `{json:true}` filter all
    /// still resolve the underlying record field. Mirrors
    /// `dbChannelTest("x.NAME$")`, `dbChannelTest("x.{}")`,
    /// `dbChannelTest("x.VAL{json:true}")` (`dbChannelTest.c:184-186`).
    #[tokio::test]
    async fn b4_local_link_accepts_field_modifiers() {
        use epics_base_rs::server::records::ai::AiRecord;
        let db = Arc::new(PvDatabase::new());
        db.add_record("REC", Box::new(AiRecord::new(1.0)))
            .await
            .unwrap();
        let resolver = install_pvalink_resolver(&db).await;
        for name in [
            "pva://REC.NAME$?local=true",          // CA long-string modifier
            "pva://REC.{}?local=true",             // empty filter → default field
            "pva://REC.VAL{json:true}?local=true", // JSON filter on a field
        ] {
            let r = resolver.open_link(name).await;
            assert!(
                r.is_ok(),
                "local link {name} with a field modifier must be accepted, got {:?}",
                r.err()
            );
        }
    }

    /// #2: a `local=true` pvalink to a QSRV group composite PV must
    /// be accepted. Group PVs live only in the QSRV provider's group
    /// registry, never the `PvDatabase`, so the record / simple-PV
    /// locality check sees nothing and would wrongly return
    /// `NotLocal`. With the QSRV provider wired via
    /// `attach_qsrv_provider`, the gate also accepts any name the
    /// provider hosts. The control case — a `local=true` link to a
    /// genuinely remote-only PV — must still be rejected.
    #[cfg(feature = "qsrv-core")]
    #[tokio::test]
    async fn b4_local_link_accepts_qsrv_group_pv() {
        use crate::qsrv::BridgeProvider;
        use epics_base_rs::server::records::ai::AiRecord;

        // Backing records for the group's two members.
        let db = Arc::new(PvDatabase::new());
        db.add_record("GRP:level", Box::new(AiRecord::new(1.0)))
            .await
            .unwrap();
        db.add_record("GRP:count", Box::new(AiRecord::new(2.0)))
            .await
            .unwrap();

        // Register a QSRV group composite PV named `LOCAL:GROUP`.
        const GROUP_JSON: &str = r#"{
            "LOCAL:GROUP": {
                "+id": "epics:nt/NTGroup:1.0",
                "level": { "+channel": "GRP:level.VAL", "+type": "plain" },
                "count": { "+channel": "GRP:count.VAL", "+type": "plain" }
            }
        }"#;
        let provider = Arc::new(BridgeProvider::new(db.clone()));
        provider.load_group_config(GROUP_JSON).expect("load group");
        provider.process_groups().await;
        assert!(
            provider.has_group_pv("LOCAL:GROUP"),
            "group PV must be registered in the provider"
        );

        let resolver = install_pvalink_resolver(&db).await;
        resolver.attach_qsrv_provider(provider);

        // local=true link to the QSRV group composite PV — accepted.
        let r = resolver.open_link("pva://LOCAL:GROUP?local=true").await;
        assert!(
            r.is_ok(),
            "local link to a QSRV group composite PV must be accepted, got err {:?}",
            r.err()
        );

        // Control: a local=true link to a genuinely remote-only PV
        // — neither a DB record/simple PV nor a QSRV channel — is
        // still rejected with NotLocal.
        let remote = resolver
            .open_link("pva://OFF:SITE:REMOTE:PV?local=true")
            .await;
        assert!(
            matches!(remote, Err(PvaLinkError::NotLocal(_))),
            "local link to a remote-only PV must still be rejected, got err {:?}",
            remote.err()
        );
    }

    // ---- DB JSON pvalink options preserved ----

    /// JSON-object pvalink options (field, proc, sevr, Q, …) survive the
    /// parse→bridge pipeline as STRUCTURED JLink members, not a synthetic
    /// `?key=value` query.
    ///
    /// Three parts: (a) parse_link_v2 yields a `ParsedLink::PvaJson` whose
    /// `options` is the verbatim `(key, value)` pair list; (b)
    /// `PvaLinkConfig::from_jlink_options` reconstructs the config from
    /// those members with no query round-trip; (c) the integration layer
    /// wires them via `open_json_link_for_record`, keyed by the bare PV
    /// name — the same path install_pvalink_resolver's pre-scanner follows
    /// for a loaded record.
    ///
    /// Upstream parity: pvxs/ioc/pvalink_jlif.cpp:69-196 (all pvalink JSON
    /// keys parsed as JLink map keys / typed values and stored on the
    /// jlink struct for the link's lifetime); :286-304 (no `?key=value`
    /// URI query parser exists in the JLink callback table).
    #[tokio::test]
    async fn br_r10_db_json_pvalink_options_preserved() {
        use epics_base_rs::server::record::{
            JlinkValue, ParsedLink, PvaJsonLink, parse_link_v2, pvajson_identity_key,
        };

        // Part 1: the JSON longhand parses to a structured PvaJson link —
        // options as JLink members, in source order with original key
        // case and JSON value KIND, never a query string.
        let json =
            r#"{pva: {pv: "TARGET:AI", field: "display.precision", proc: "CPP", sevr: "MS"}}"#;
        let j = match parse_link_v2(json) {
            ParsedLink::PvaJson(j) => j,
            other => panic!("expected PvaJson, got {other:?}"),
        };
        assert_eq!(
            *j,
            PvaJsonLink::new(
                "TARGET:AI".to_string(),
                vec![
                    (
                        "field".to_string(),
                        JlinkValue::Str("display.precision".to_string())
                    ),
                    ("proc".to_string(), JlinkValue::Str("CPP".to_string())),
                    ("sevr".to_string(), JlinkValue::Str("MS".to_string())),
                ],
            ),
            "pvalink options preserved as structured JLink members"
        );

        // Part 2: reconstruct the config straight from the structured
        // members — the single `apply_options` owner, no `?query` parse.
        let cfg = PvaLinkConfig::from_jlink_options(&j.pv, &j.options, LinkDirection::Inp).unwrap();
        assert_eq!(cfg.pv_name, "TARGET:AI");
        assert_eq!(cfg.field, "display.precision");
        assert!(cfg.scan_on_update, "CPP → scan_on_update");
        assert!(cfg.scan_on_passive, "CPP → scan_on_passive");
        assert_eq!(cfg.sevr, SevrMode::Ms);

        // Part 3: the integration layer wires the structured options
        // exactly as install_pvalink_resolver's pre-scanner does for a
        // loaded record carrying this PvaJson link.
        let resolver = PvaLinkResolver::new(crate::test_reactor());
        let _ = resolver
            .open_json_link_for_record(&j.pv, &j.options, inp("MY:RECORD"))
            .await;

        // options are registered under the per-link IDENTITY KEY — the
        // string epics-base-rs hands the lset for a PvaJson link
        // (`external_pv_name()` → `link_identity_key()` →
        // `resolve_external_pv(&key)`), so the steady-state hot path
        // resolves the config by that same key. Computing it from the
        // same `(pv, options)` proves base and bridge agree.
        let key = pvajson_identity_key(&j.pv, &j.options);
        let cfg = resolver.inp_cfg_for(&key);
        assert_eq!(
            cfg.field, "display.precision",
            "field option must be registered (was 'value' before fix)"
        );
        assert_eq!(cfg.sevr, SevrMode::Ms, "sevr option must be registered");
        assert!(cfg.scan_on_update, "CPP scan_on_update must be registered");
        assert!(
            cfg.scan_on_passive,
            "CPP scan_on_passive must be registered"
        );

        // The bare PV is recovered from the identity key for channel
        // sharing — `link_pv_name` strips the separator + encoded options.
        assert_eq!(link_pv_name(&key), "TARGET:AI");

        // Scan target must be registered under the bare PV name.
        let targets = resolver.scan_targets.read();
        let fanout = targets
            .get(&mk("TARGET:AI"))
            .expect("CPP target must be registered");
        assert_eq!(fanout.records[0].record, "MY:RECORD");
        assert!(fanout.records[0].passive_only, "CPP must set passive_only");
        // The per-link `field` selector lives in the link config
        // (applied at read time), asserted via `cfg.field` above — pvxs
        // resolves `fieldName` per-link at the lset getter, not in the
        // scan fan-out.
    }

    /// a DB record can write a pvalink in the *legacy
    /// whitespace-suffix* form — `pva://TARGET:AI CPP MS` — instead of
    /// the JSON `?key=value` query form. Before the fix the bare-name
    /// extraction (`strip_query`) left the suffix attached, so
    /// `"TARGET:AI CPP MS"` became the upstream channel identity and the
    /// `default_inp_cfg` pv_name; the `CPP`/`MS` modifiers were never
    /// interpreted, and the install-time pre-scanner's `!s.contains('?')`
    /// gate skipped the link entirely (no CP/CPP scan target wired).
    ///
    /// After the fix `link_pv_name` collapses BOTH option representations
    /// to the bare PV identity, so the existing parse-based registration
    /// honours the suffix uniformly: `PvaLinkConfig::parse`
    /// (`strip_legacy_mods`) is the single owner that interprets
    /// `CPP`/`MS`, the channel opens under `"TARGET:AI"`, and the CPP scan
    /// target lands under the bare PV name.
    ///
    /// Upstream parity:
    ///   pvxs/ioc/pvalink_jlif.cpp — legacy modifier vocabulary
    ///   pvxs/ioc/pvalink.h:65     — per-link `pvaLinkConfig`
    #[tokio::test]
    async fn br_fr9_pvalink_legacy_suffix_options_parsed_not_pv_name() {
        use epics_base_rs::server::record::{ParsedLink, parse_link_v2};

        // Part 1: the legacy suffix rides into the pvalink layer intact.
        // epics-base does NOT strip pva:// modifiers — the dual
        // representation is collapsed in the pvalink layer, not duplicated
        // in epics-base's modifier vocabulary.
        let stored = match parse_link_v2("pva://TARGET:AI CPP MS") {
            ParsedLink::Pva(s) => s.to_string(),
            other => panic!("expected Pva, got {other:?}"),
        };
        assert_eq!(
            &*stored, "TARGET:AI CPP MS",
            "epics-base must preserve the legacy suffix verbatim"
        );

        // Part 2: PvaLinkConfig::parse is the single owner that interprets
        // the suffix — PV name is the first token, modifiers become config
        // fields. (Was: whole string treated as the PV name.)
        let cfg = PvaLinkConfig::parse(&format!("pva://{stored}"), LinkDirection::Inp).unwrap();
        assert_eq!(
            cfg.pv_name, "TARGET:AI",
            "suffix must NOT fold into the PV name"
        );
        assert_eq!(cfg.sevr, SevrMode::Ms, "MS → maximize-severity");
        assert!(cfg.scan_on_update, "CPP → scan_on_update");
        assert!(cfg.scan_on_passive, "CPP → scan_on_passive");

        // Part 3: the integration layer registers options + scan target
        // for a suffix link exactly as it does for a query-bearing one.
        let resolver = PvaLinkResolver::new(crate::test_reactor());
        let _ = resolver
            .open_link_for_record(&format!("pva://{stored}"), inp("MY:RECORD"))
            .await;

        // inp_cfg_for keyed by the full suffix string returns the parsed
        // config — bare PV name, MS, CPP — not the default for a PV
        // literally named "TARGET:AI CPP MS".
        let cfg = resolver.inp_cfg_for(&stored);
        assert_eq!(cfg.pv_name, "TARGET:AI");
        assert_eq!(
            cfg.sevr,
            SevrMode::Ms,
            "sevr must be registered from the suffix"
        );
        assert!(cfg.scan_on_update, "CPP scan_on_update must be registered");
        assert!(
            cfg.scan_on_passive,
            "CPP scan_on_passive must be registered"
        );

        // Scan target lands under the BARE PV name, never the suffixed
        // form — the suffix string must never become a registry key.
        let targets = resolver.scan_targets.read();
        let fanout = targets
            .get(&mk("TARGET:AI"))
            .expect("CPP target must be registered under the bare PV name");
        assert_eq!(fanout.records[0].record, "MY:RECORD");
        assert!(fanout.records[0].passive_only, "CPP must set passive_only");
        assert!(
            targets.get(&mk("TARGET:AI CPP MS")).is_none(),
            "suffix string must never become a scan-target key"
        );
    }

    /// two links to the same upstream PV with different `field`
    /// and `proc` options must have independent cached state — no leakage
    /// of one link's options into the other's config or scan targets.
    ///
    /// Fails on main: both links land in `link_options["TARGET:PV"]`
    /// (last write wins), so the first link's config is overwritten and
    /// the `inp_cfg_for` lookup returns the second link's field for both.
    ///
    /// Upstream parity:
    ///   pvxs/ioc/pvalink.h:65    — `pvaLinkConfig` is per-link
    ///   pvxs/ioc/pvalink.h:116   — channel key = (channelName, pvRequest)
    ///   pvxs/ioc/pvalink_link.cpp:91 — `root = lchan->root[fieldName]`
    #[tokio::test]
    async fn br_r27_pvalink_cache_separates_per_link_options() {
        let resolver = PvaLinkResolver::new(crate::test_reactor());

        // Link A: read sub-field "alarm.severity", CPP (scan on passive).
        let link_a = "pva://TARGET:PV?field=alarm.severity&proc=CPP";
        let _ = resolver.open_link_for_record(link_a, inp("RECORD:A")).await;

        // Link B: read sub-field "value", CP (always scan).
        let link_b = "pva://TARGET:PV?field=value&proc=CP";
        let _ = resolver.open_link_for_record(link_b, inp("RECORD:B")).await;

        // Each link must have its own config — no cross-contamination.
        let cfg_a = resolver.inp_cfg_for("TARGET:PV?field=alarm.severity&proc=CPP");
        let cfg_b = resolver.inp_cfg_for("TARGET:PV?field=value&proc=CP");

        assert_eq!(
            cfg_a.field, "alarm.severity",
            "link A field must not be overwritten by link B"
        );
        assert_eq!(
            cfg_b.field, "value",
            "link B field must retain its own value"
        );
        assert!(
            cfg_a.scan_on_passive,
            "link A CPP must set scan_on_passive; link B's CP must not clobber it"
        );
        assert!(
            !cfg_b.scan_on_passive,
            "link B CP must not be passive-only; link A must not propagate"
        );

        // ScanTargets must also be independent per-link — each record
        // gets its own entry with its own proc mode (CPP vs CP). The
        // per-link `field` is verified at the `cfg` level above (it is
        // applied at read time, not in the scan fan-out).
        let targets = resolver.scan_targets.read();
        let fanout = targets
            .get(&mk("TARGET:PV"))
            .expect("scan targets registered for TARGET:PV");
        let rec_a = fanout
            .records
            .iter()
            .find(|t| t.record == "RECORD:A")
            .expect("RECORD:A must be in scan targets");
        let rec_b = fanout
            .records
            .iter()
            .find(|t| t.record == "RECORD:B")
            .expect("RECORD:B must be in scan targets");
        assert!(rec_a.passive_only, "RECORD:A must be CPP (passive_only)");
        assert!(
            !rec_b.passive_only,
            "RECORD:B must be CP (not passive_only)"
        );
    }

    /// The STRUCTURED-JSON counterpart of
    /// [`Self::br_r27_pvalink_cache_separates_per_link_options`]: two
    /// `{pva:{pv:"TARGET:PV", …}}` links to the SAME PV that differ only
    /// by their JLink options must keep independent per-link configs and
    /// scan fan-outs.
    ///
    /// Before the fix the JSON path keyed `link_options` by the bare PV
    /// (`j.pv`), so the second link's config overwrote the first
    /// (last-writer-wins) and `resolve_external_pv(&j.pv)` returned the
    /// same config for both records. The structural fix mints a per-link
    /// identity key (`pvajson_identity_key`) on BOTH sides of the lset
    /// boundary — base via `external_pv_name()` → `link_identity_key()`,
    /// bridge at registration — so the two links never collide, while the
    /// monitor channel is still shared by bare PV + `(pipeline, Q)`.
    ///
    /// Upstream parity:
    ///   pvxs/ioc/pvalink.h:65    — `pvaLinkConfig` is per-link
    ///   pvxs/ioc/pvalink.h:116   — channel key = (channelName, pvRequest)
    #[tokio::test]
    async fn br_json_links_same_pv_distinct_options_do_not_collide() {
        use epics_base_rs::server::record::pvajson_identity_key;

        let resolver = PvaLinkResolver::new(crate::test_reactor());

        // Link A: read "alarm.severity", CPP (scan on passive).
        let opts_a: Vec<(String, JlinkValue)> = vec![
            (
                "field".to_string(),
                JlinkValue::Str("alarm.severity".to_string()),
            ),
            ("proc".to_string(), JlinkValue::Str("CPP".to_string())),
        ];
        // Link B: read "value", CP (always scan).
        let opts_b: Vec<(String, JlinkValue)> = vec![
            ("field".to_string(), JlinkValue::Str("value".to_string())),
            ("proc".to_string(), JlinkValue::Str("CP".to_string())),
        ];
        let _ = resolver
            .open_json_link_for_record("TARGET:PV", &opts_a, inp("RECORD:A"))
            .await;
        let _ = resolver
            .open_json_link_for_record("TARGET:PV", &opts_b, inp("RECORD:B"))
            .await;

        // Each link resolves its own config by its identity key — the same
        // key base hands the lset at resolve time.
        let key_a = pvajson_identity_key("TARGET:PV", &opts_a);
        let key_b = pvajson_identity_key("TARGET:PV", &opts_b);
        assert_ne!(key_a, key_b, "distinct options must mint distinct keys");
        let cfg_a = resolver.inp_cfg_for(&key_a);
        let cfg_b = resolver.inp_cfg_for(&key_b);
        assert_eq!(
            cfg_a.field, "alarm.severity",
            "link A field must not be overwritten by link B (was the bug)"
        );
        assert_eq!(cfg_b.field, "value", "link B field must retain its own");
        assert!(
            cfg_a.scan_on_passive,
            "link A CPP must set scan_on_passive; link B's CP must not clobber it"
        );
        assert!(
            !cfg_b.scan_on_passive,
            "link B CP must not be passive-only; link A must not propagate"
        );

        // The shared channel identity is still the bare PV for both.
        assert_eq!(link_pv_name(&key_a), "TARGET:PV");
        assert_eq!(link_pv_name(&key_b), "TARGET:PV");

        // Scan fan-out for the shared (bare PV, default Q) monitor holds
        // both records, each with its own proc mode.
        let targets = resolver.scan_targets.read();
        let fanout = targets
            .get(&mk("TARGET:PV"))
            .expect("scan targets registered for TARGET:PV");
        let rec_a = fanout
            .records
            .iter()
            .find(|t| t.record == "RECORD:A")
            .expect("RECORD:A must be in scan targets");
        let rec_b = fanout
            .records
            .iter()
            .find(|t| t.record == "RECORD:B")
            .expect("RECORD:B must be in scan targets");
        assert!(rec_a.passive_only, "RECORD:A must be CPP (passive_only)");
        assert!(
            !rec_b.passive_only,
            "RECORD:B must be CP (not passive_only)"
        );
    }

    /// Structured-JSON links to the same PV that differ by `Q` are
    /// distinct monitor variants (distinct pvxs subscriptions,
    /// `pvxs/ioc/pvalink.h:116-120`), exactly as the convenience-URI form
    /// in [`Self::br128_distinct_q_variants_do_not_collapse`]. Their CP
    /// scan fan-outs must land in SEPARATE [`MonitorKey`] buckets even
    /// though both arrive through the JSON identity-key boundary.
    #[tokio::test]
    async fn br_json_distinct_q_variants_do_not_collapse() {
        let resolver = PvaLinkResolver::new(crate::test_reactor());

        let opts_q1: Vec<(String, JlinkValue)> = vec![
            ("proc".to_string(), JlinkValue::Str("CP".to_string())),
            ("Q".to_string(), JlinkValue::Int(1)),
        ];
        let opts_q64: Vec<(String, JlinkValue)> = vec![
            ("proc".to_string(), JlinkValue::Str("CP".to_string())),
            ("Q".to_string(), JlinkValue::Int(64)),
        ];
        let _ = resolver
            .open_json_link_for_record("VAR:PV", &opts_q1, inp("REC:Q1"))
            .await;
        let _ = resolver
            .open_json_link_for_record("VAR:PV", &opts_q64, inp("REC:Q64"))
            .await;

        let targets = resolver.scan_targets.read();
        let key_q1 = MonitorKey {
            pv_name: "VAR:PV".to_string(),
            pipeline: false,
            queue_size: 1,
        };
        let key_q64 = MonitorKey {
            pv_name: "VAR:PV".to_string(),
            pipeline: false,
            queue_size: 64,
        };
        let fan_q1 = targets
            .get(&key_q1)
            .expect("Q=1 variant must have its own scan fan-out");
        let fan_q64 = targets
            .get(&key_q64)
            .expect("Q=64 variant must have its own scan fan-out");
        assert_eq!(fan_q1.records.len(), 1, "Q=1 fan-out holds only its record");
        assert_eq!(fan_q1.records[0].record, "REC:Q1");
        assert_eq!(
            fan_q64.records.len(),
            1,
            "Q=64 fan-out must not inherit the Q=1 record"
        );
        assert_eq!(fan_q64.records[0].record, "REC:Q64");
    }

    /// A structured-JSON OUT link's options are registered under the
    /// per-link identity key (matching `external_pv_name()` →
    /// `link_identity_key()` for a `PvaJson` OUT link in
    /// `epics_base_rs::server::database::links`), so two same-PV OUT links
    /// with different `field`/`proc` keep distinct configs on the
    /// `put_value` resolver hot path.
    #[tokio::test]
    async fn br_json_out_links_same_pv_distinct_options_do_not_collide() {
        use epics_base_rs::server::record::pvajson_identity_key;

        let resolver = PvaLinkResolver::new(crate::test_reactor());

        let opts_a: Vec<(String, JlinkValue)> =
            vec![("field".to_string(), JlinkValue::Str("a.b".to_string()))];
        let opts_b: Vec<(String, JlinkValue)> =
            vec![("field".to_string(), JlinkValue::Str("c.d".to_string()))];
        let _ = resolver
            .open_json_out_link("OUT:PV", &opts_a, Some(out(b"WRITER:A")))
            .await;
        let _ = resolver
            .open_json_out_link("OUT:PV", &opts_b, Some(out(b"WRITER:B")))
            .await;

        let key_a = pvajson_identity_key("OUT:PV", &opts_a);
        let key_b = pvajson_identity_key("OUT:PV", &opts_b);
        let cfg_a = resolver.out_cfg_for(&key_a);
        let cfg_b = resolver.out_cfg_for(&key_b);
        assert_eq!(cfg_a.field, "a.b", "OUT link A field must not be clobbered");
        assert_eq!(cfg_b.field, "c.d", "OUT link B field must retain its own");
        assert_eq!(link_pv_name(&key_a), "OUT:PV");
    }

    /// Two records linking the SAME upstream
    /// PV with different `Q` are distinct monitor variants (distinct
    /// pvxs subscriptions, `pvxs/ioc/pvalink.h:116-120`). Their CP scan
    /// fan-out must land in SEPARATE [`MonitorKey`] buckets — pre-fix
    /// both collapsed onto one bare-PV-name entry, so a `Q=1` record
    /// could be driven by the `Q=64` monitor's events, and only one
    /// forwarder spawned for both. The base iocInit wait
    /// (`link_names`) must likewise list BOTH variants so each monitor's
    /// connection is awaited independently.
    #[tokio::test]
    async fn br128_distinct_q_variants_do_not_collapse() {
        let resolver = PvaLinkResolver::new(crate::test_reactor());

        // Same PV, CP scan, but different queue depths → two variants.
        let _ = resolver
            .open_link_for_record("pva://VAR:PV?proc=CP&Q=1", inp("REC:Q1"))
            .await;
        let _ = resolver
            .open_link_for_record("pva://VAR:PV?proc=CP&Q=64", inp("REC:Q64"))
            .await;

        {
            let targets = resolver.scan_targets.read();
            let key_q1 = MonitorKey {
                pv_name: "VAR:PV".to_string(),
                pipeline: false,
                queue_size: 1,
            };
            let key_q64 = MonitorKey {
                pv_name: "VAR:PV".to_string(),
                pipeline: false,
                queue_size: 64,
            };
            let fan_q1 = targets
                .get(&key_q1)
                .expect("Q=1 variant must have its own scan fan-out");
            let fan_q64 = targets
                .get(&key_q64)
                .expect("Q=64 variant must have its own scan fan-out");
            assert_eq!(
                fan_q1.records.len(),
                1,
                "Q=1 fan-out must hold only its own record"
            );
            assert_eq!(fan_q1.records[0].record, "REC:Q1");
            assert_eq!(
                fan_q64.records.len(),
                1,
                "Q=64 fan-out must not inherit the Q=1 record"
            );
            assert_eq!(fan_q64.records[0].record, "REC:Q64");
        }

        // Both monitor variants must appear in the iocInit wait set,
        // each rendered as an identity that round-trips back to its own
        // variant; the default-Q form is bare, non-default carries the
        // query.
        let mut names = resolver.link_names();
        names.sort();
        assert_eq!(
            names,
            vec![
                "VAR:PV?pipeline=false&Q=1".to_string(),
                "VAR:PV?pipeline=false&Q=64".to_string(),
            ],
            "both Q variants must be waited on independently"
        );
    }

    /// The `pipeline` dimension of the monitor
    /// identity must also separate variants — a default link and a
    /// `Q=1&pipeline=true` link to the same PV are distinct monitor
    /// channels (distinct pvRequest, `pvxs/ioc/pvalink_link.cpp:49-65`,
    /// `pvxs/ioc/pvalink_lset.cpp:99-131`). Each must open its OWN
    /// cached link (its own monitor receiver) and register its scan
    /// fan-out under its own [`MonitorKey`], rather than the default
    /// link satisfying both reads and CP scans for the pipelined one.
    #[tokio::test]
    async fn br37_pipeline_variant_opens_its_own_monitor() {
        let resolver = PvaLinkResolver::new(crate::test_reactor());

        // Default variant (no query options) and a pipelined Q=1 variant
        // of the SAME PV, both CP so each registers a scan target.
        let _ = resolver
            .open_link_for_record("pva://PIPE:PV?proc=CP", inp("REC:DEF"))
            .await;
        let _ = resolver
            .open_link_for_record("pva://PIPE:PV?proc=CP&Q=1&pipeline=true", inp("REC:PIPE"))
            .await;

        let default_q = PvaLinkConfig::defaults_for("", LinkDirection::Inp).queue_size;

        // Each variant opened its OWN cached link (own monitor receiver).
        let link_def = resolver
            .registry
            .try_get_inp("PIPE:PV", false, default_q)
            .expect("default variant link must exist");
        let link_pipe = resolver
            .registry
            .try_get_inp("PIPE:PV", true, 1)
            .expect("pipelined Q=1 variant link must exist");
        assert!(
            !Arc::ptr_eq(&link_def, &link_pipe),
            "pipeline/Q-distinct variants must not share one monitor"
        );

        // Scan fan-out is keyed per variant: each holds only its own
        // record, never the sibling's.
        let targets = resolver.scan_targets.read();
        let fan_def = targets
            .get(&MonitorKey {
                pv_name: "PIPE:PV".to_string(),
                pipeline: false,
                queue_size: default_q,
            })
            .expect("default variant scan fan-out");
        let fan_pipe = targets
            .get(&MonitorKey {
                pv_name: "PIPE:PV".to_string(),
                pipeline: true,
                queue_size: 1,
            })
            .expect("pipelined variant scan fan-out");
        assert_eq!(fan_def.records.len(), 1);
        assert_eq!(fan_def.records[0].record, "REC:DEF");
        assert_eq!(fan_pipe.records.len(), 1);
        assert_eq!(fan_pipe.records[0].record, "REC:PIPE");
    }

    /// #2: with no QSRV provider wired (pvalink-only deployment), the
    /// `local` gate keeps its record / simple-PV behaviour — group-PV
    /// locality is simply unavailable, and a link to a non-local PV
    /// is still rejected. Guards the optionality of the QSRV handle.
    #[cfg(feature = "qsrv-core")]
    #[tokio::test]
    async fn b4_local_gate_without_qsrv_still_rejects_remote() {
        let db = Arc::new(PvDatabase::new());
        let resolver = install_pvalink_resolver(&db).await;
        // No attach_qsrv_provider call.
        let r = resolver
            .open_link("pva://NO:QSRV:REMOTE:PV?local=true")
            .await;
        assert!(
            matches!(r, Err(PvaLinkError::NotLocal(_))),
            "without a QSRV handle a non-local link must still be rejected"
        );
    }

    /// A record whose `process()` logs its name. The first atomic
    /// target additionally fires a `Notify` *after* logging — at that
    /// instant the forwarder is provably inside its multi-record
    /// epoch — so the Regression test can release a competing
    /// writer into a guaranteed-contended window.
    struct SlowLoggingRecord {
        name: &'static str,
        log: Arc<parking_lot::Mutex<Vec<String>>>,
        /// Fired once, from inside the epoch, by whichever record
        /// carries it. `None` for records that should not signal.
        epoch_entered: Option<Arc<tokio::sync::Notify>>,
    }

    impl epics_base_rs::server::record::Record for SlowLoggingRecord {
        fn record_type(&self) -> &'static str {
            "ai"
        }
        fn process(
            &mut self,
        ) -> epics_base_rs::error::CaResult<epics_base_rs::server::record::ProcessOutcome> {
            self.log.lock().push(self.name.to_string());
            if let Some(n) = &self.epoch_entered {
                // The forwarder already holds the epoch before this
                // record's body runs; signalling here releases the
                // competing writer into a window the epoch must
                // exclude.
                n.notify_one();
            }
            // Hold the worker briefly so a competing task on another
            // worker thread has a genuine window to acquire an epoch
            // lock if one is not already excluding it.
            std::thread::sleep(std::time::Duration::from_millis(40));
            Ok(epics_base_rs::server::record::ProcessOutcome::complete())
        }
        fn get_field(&self, _name: &str) -> Option<EpicsValue> {
            Some(EpicsValue::Double(0.0))
        }
        fn put_field(
            &mut self,
            _name: &str,
            _value: EpicsValue,
        ) -> epics_base_rs::error::CaResult<()> {
            Ok(())
        }
        fn declared_fields(&self) -> &'static [epics_base_rs::server::record::FieldDesc] {
            &[]
        }
    }

    /// the pvalink `atomic` scan-on-update forwarder must hold
    /// a single locked scan epoch over the atomic target record set,
    /// so no other writer can interleave *between* the atomic
    /// targets. Mirrors pvxs `DBManyLocker L(atomic_lock)` held across
    /// the atomic scan in `pvxs/ioc/pvalink_channel.cpp:423`.
    ///
    /// The forwarder scans an atomic group {A, B}. The first atomic
    /// target (A) signals `epoch_entered` from inside its body — the
    /// forwarder provably holds the multi-record epoch at that point.
    /// Only then is a competing task — standing in for a direct
    /// record writer or a second atomic scan — released to enter its
    /// own epoch over record B. With the epoch held the competing
    /// task is blocked until the *whole* atomic group has scanned, so
    /// the observed order is `A, B, EXTERNAL`. Without the epoch the
    /// competing writer lands between A and B.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn br_r18_atomic_scan_holds_multi_record_lock_epoch() {
        let db = PvDatabase::new();
        let log = Arc::new(parking_lot::Mutex::new(Vec::new()));
        let epoch_entered = Arc::new(tokio::sync::Notify::new());
        for name in ["AT:A", "AT:B"] {
            db.add_record(
                name,
                Box::new(SlowLoggingRecord {
                    name,
                    log: log.clone(),
                    // Only A signals — by the time A's body runs the
                    // epoch over {A, B} is already held.
                    epoch_entered: (name == "AT:A").then(|| epoch_entered.clone()),
                }),
            )
            .await
            .unwrap();
        }

        // Atomic group: A (monorder 0) then B (monorder 1).
        let mut fanout = ScanFanout::default();
        fanout.records.push(ScanTarget {
            field: "INP".into(),
            record: "AT:A".into(),
            monorder: 0,
            atomic: true,
            passive_only: false,
        });
        fanout.records.push(ScanTarget {
            field: "INP".into(),
            record: "AT:B".into(),
            monorder: 1,
            atomic: true,
            passive_only: false,
        });
        let scan_targets: ScanTargetMap =
            Arc::new(parking_lot::RwLock::new(std::collections::HashMap::from([
                (mk("SRC"), fanout),
            ])));
        let db_slot = Arc::new(parking_lot::RwLock::new(Some(db.clone())));

        let (tx, rx) = tokio::sync::mpsc::channel::<ScanEvent>(4);
        let forwarder = tokio::spawn(run_notify_forwarder(
            mk("SRC"),
            rx,
            scan_targets,
            db_slot,
            Arc::new(ScanOverrun::default()),
        ));

        // Competing party: contends for an epoch over an atomic
        // target record (AT:B). It waits until the forwarder is
        // provably inside the epoch (A's body fired `epoch_entered`),
        // then attempts its own epoch and records `EXTERNAL` only
        // once it actually owns it.
        let competitor_log = log.clone();
        let competitor_db = db.clone();
        let competitor = tokio::spawn(async move {
            epoch_entered.notified().await;
            let _epoch = competitor_db.lock_records(&["AT:B".to_string()]);
            competitor_log.lock().push("EXTERNAL".to_string());
        });

        tx.send(ScanEvent::Value(nt_scalar(1.0))).await.unwrap();
        drop(tx);
        forwarder.await.unwrap();
        competitor.await.unwrap();

        // The atomic group {A, B} scans as one epoch; the competing
        // epoch can only be granted after the group completes.
        assert_eq!(
            *log.lock(),
            vec!["AT:A", "AT:B", "EXTERNAL"],
            "external writer must not interleave between atomic scan targets"
        );
    }

    /// the alarm / time / metadata getters must apply the
    /// *caller's* per-link `sevr` / `time` / `field` options, not the
    /// shared cached `PvaLink`'s config.
    ///
    /// The registry caches one INP `PvaLink` per
    /// `(pv_name, pipeline, queue_size, direction)` — `sevr` / `time`
    /// / `field` are not in the key. Pre-fix the getters either used
    /// `default_inp_cfg` (discarding the caller's options) or read
    /// the cached link's `config.*`, so a second caller with
    /// different options got the first caller's behavior.
    ///
    /// Here the cached INP link is configured `sevr=NMS` (default),
    /// `time=false`, `field="value"`, with a remote value carrying a
    /// MAJOR alarm and a timeStamp. A caller asking for `?sevr=MS` /
    /// `?time=true` must observe its own options against that shared
    /// cached link. pvxs `pvaLinkConfig` is per-link
    /// (`pvxs/ioc/pvalink.h:65`).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn mr_r15_getters_use_caller_options_not_shared_link() {
        use crate::pvalink::link::PvaLink;
        use epics_pva_rs::pvdata::PvField;

        // Remote NT value: MAJOR alarm + a timeStamp.
        let mut alarm = PvStructure::new("alarm_t");
        alarm
            .fields
            .push(("severity".into(), PvField::Scalar(ScalarValue::Int(2))));
        alarm.fields.push((
            "message".into(),
            PvField::Scalar(ScalarValue::String("HIGH".into())),
        ));
        let mut ts = PvStructure::new("time_t");
        ts.fields.push((
            "secondsPastEpoch".into(),
            PvField::Scalar(ScalarValue::Long(1_700_000_000)),
        ));
        ts.fields
            .push(("nanoseconds".into(), PvField::Scalar(ScalarValue::Int(42))));
        // bit-31 userTag: confirms the zero-extended 64-bit tag survives
        // the full resolver/trait chain, not just the link-level read.
        ts.fields.push((
            "userTag".into(),
            PvField::Scalar(ScalarValue::Int(0x9000_0000u32 as i32)),
        ));
        let mut root = PvStructure::new("epics:nt/NTScalar:1.0");
        root.fields
            .push(("value".into(), PvField::Scalar(ScalarValue::Double(3.0))));
        root.fields
            .push(("alarm".into(), PvField::Structure(alarm)));
        root.fields
            .push(("timeStamp".into(), PvField::Structure(ts)));
        let cached = PvField::Structure(root);

        let resolver = PvaLinkResolver::new(crate::test_reactor());

        // Seed the registry with a cached INP link whose own config
        // is the bare default: NMS, time=false, field="value".
        let shared_cfg = default_inp_cfg("MR_R15:PV");
        let shared_link = std::sync::Arc::new(PvaLink::for_test(shared_cfg.clone(), Some(cached)));
        resolver.registry.insert_for_test(&shared_cfg, shared_link);

        // The shared cached link is NMS → its own gate reports no
        // alarm. A caller asking for `?sevr=MS` must still see the
        // MAJOR severity propagate.
        let nms_name = "pva://MR_R15:PV";
        assert_eq!(
            LinkSet::alarm_severity(&resolver, nms_name.as_bytes()),
            None,
            "an NMS caller must not propagate the remote alarm"
        );
        let ms_name = "pva://MR_R15:PV?sevr=MS";
        assert_eq!(
            LinkSet::alarm_severity(&resolver, ms_name.as_bytes()),
            Some(2),
            "an MS caller must see MAJOR even though the cached link is NMS"
        );
        assert_eq!(
            LinkSet::alarm_message(&resolver, ms_name.as_bytes()).as_deref(),
            Some("HIGH"),
            "an MS caller must get the remote alarm message"
        );
        assert_eq!(
            resolver.link_alarm_severity(ms_name),
            Some(2),
            "resolver-level link_alarm_severity must use the caller's MS mode"
        );

        // Ungated snapshot (pvxs pvaGetAlarmMsg / dbGetAlarm). The
        // snapshot is LATCHED
        // at the value read, not read live from the cached value. BEFORE
        // any value read it is the pvxs initial INVALID_ALARM(3) /
        // LINK_ALARM(14) / blank (`pvxs/ioc/pvalink.h:250`) — exactly like
        // `dbGetAlarm` before the first `dbGetLink`
        // (`pvxs/test/testpvalink.cpp:373-375`). The earlier
        // `alarm_severity` calls read the cached value live and do NOT
        // latch, so the snapshot is still INVALID here.
        let pre = LinkSet::remote_alarm(&resolver, nms_name.as_bytes())
            .expect("connected link reports the initial INVALID snapshot");
        assert_eq!(pre.severity, 3, "pre-read snapshot is INVALID_ALARM");
        assert_eq!(pre.status, 14, "LINK_ALARM status");
        assert_eq!(&*pre.message, "", "initial snapshot carries no message");

        // A value read (the LinkSet `get_value` path = pvxs `dbGetLink` /
        // `pvaGetValue`) latches the snapshot.
        assert!(
            LinkSet::get_value(&resolver, nms_name.as_bytes())
                .await
                .is_some(),
            "the cached value read must succeed and latch the snapshot"
        );

        // AFTER the value read: even the NMS caller — whose gated
        // contribution is None above — sees the remote MAJOR severity,
        // LINK_ALARM(14) status, and message. The `sevr` mode does not
        // gate this path.
        let snap = LinkSet::remote_alarm(&resolver, nms_name.as_bytes())
            .expect("NMS caller still gets the ungated remote alarm snapshot");
        assert_eq!(snap.severity, 2, "ungated snapshot reports remote MAJOR");
        assert_eq!(snap.status, 14, "LINK_ALARM status derived from severity");
        assert_eq!(
            &*snap.message, "HIGH",
            "ungated snapshot carries the message"
        );

        // The shared cached link is time=false → a bare caller adopts
        // no timestamp. A caller asking `?time=true` must adopt it.
        assert_eq!(
            LinkSet::time_stamp(&resolver, nms_name.as_bytes()),
            None,
            "a time=false caller must not adopt the upstream timestamp"
        );
        assert_eq!(
            LinkSet::time_stamp(&resolver, b"pva://MR_R15:PV?time=true"),
            Some((1_700_000_000, 42, 0x0000_0000_9000_0000)),
            "a time=true caller must adopt the upstream timestamp and the \
             zero-extended userTag"
        );
    }

    #[cfg(tokio_backend)]
    /// an OUT pvalink fed by an `EpicsValue::UInt64Array`
    /// (from an `FTVL=UINT64` waveform) must go through the typed
    /// `ulong[]` encoder, not the scalar string-PUT path. Pre-fix the
    /// OUT dispatcher's hard-coded `array_path` match omitted
    /// `UInt64Array`, so the value fell through to `value.to_string()`
    /// — a bracketed `[1, 2]` literal the PVA array string parser
    /// rejects.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn mr_r23_out_uint64_array_uses_typed_path() {
        use epics_pva_rs::pvdata::ScalarType;
        out_array_typed_path_case(
            "MR_R23:PV",
            ScalarType::ULong,
            EpicsValue::UInt64Array(vec![1, 2, u64::MAX].into()),
            // u64::MAX as the i64 bit pattern is -1; the test
            // compares 64-bit words, so full-width u64 is preserved.
            &[1, 2, u64::MAX as i64],
        )
        .await;
    }

    #[cfg(tokio_backend)]
    /// an OUT pvalink fed by an `EpicsValue::Int64Array`
    /// (from an `FTVL=INT64` waveform) must go through the typed
    /// `long[]` encoder. `origin/main` already had `Int64Array` but
    /// the OUT dispatcher's `array_path` match never listed it, so a
    /// valid int64 waveform value attempted to replay its bracketed
    /// `Display` string as a PVA array literal the parser rejects.
    /// The `is_array_value` helper covers signed
    /// and unsigned 64-bit arrays together.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn ex_r10_out_int64_array_uses_typed_path() {
        use epics_pva_rs::pvdata::ScalarType;
        out_array_typed_path_case(
            "EX_R10:PV",
            ScalarType::Long,
            EpicsValue::Int64Array(vec![-3, 0, i64::MAX].into()),
            &[-3, 0, i64::MAX],
        )
        .await;
    }

    #[cfg(tokio_backend)]
    /// Shared body for the OUT typed-array
    /// regression tests. Stands up a PVA server hosting a
    /// `long[]` / `ulong[]` PV, seeds the registry with a
    /// pinned-client OUT `PvaLink`, and drives `LinkSet::put_value`
    /// with `value`. The PUT must succeed and the server PV must
    /// hold `expected` (compared as `i64` bit patterns regardless of
    /// the wire-level signedness).
    async fn out_array_typed_path_case(
        pv_name: &str,
        elem_type: epics_pva_rs::pvdata::ScalarType,
        value: EpicsValue,
        expected: &[i64],
    ) {
        use crate::pvalink::link::PvaLink;
        use epics_pva_rs::client::PvaClient;
        use epics_pva_rs::pvdata::{FieldDesc, PvField, ScalarValue};
        #[cfg(tokio_backend)]
        use epics_pva_rs::server_native::PvaServer;
        use epics_pva_rs::server_native::{SharedPV, SharedSource};

        // PVA server hosting a `long[]` / `ulong[]`-valued PV.
        let desc = FieldDesc::Structure {
            struct_id: "epics:nt/NTScalarArray:1.0".into(),
            fields: vec![("value".into(), FieldDesc::ScalarArray(elem_type))].into(),
        };
        let initial = PvField::Structure(epics_pva_rs::pvdata::PvStructure {
            struct_id: "epics:nt/NTScalarArray:1.0".into(),
            fields: vec![("value".into(), PvField::ScalarArray(vec![]))],
        });
        // A writable mailbox PV: a plain `SharedPV::new()` rejects every
        // PUT ("PUT not supported by this PV" — pvxs `sharedpv.cpp:209-227`
        // makes a handler-less SharedPV non-writable). The typed PUT must
        // land and store so the readback below can verify the typed
        // encoder produced the right elements.
        let pv = SharedPV::build_mailbox();
        pv.open(desc, initial).unwrap();
        let source = SharedSource::new();
        source.add(pv_name, pv.clone());
        let server =
            PvaServer::isolated(std::sync::Arc::new(source)).expect("test PVA server starts");
        let addr = server.tcp_addr();

        let resolver = PvaLinkResolver::new(crate::test_reactor());

        // Seed the registry with a pinned-client OUT link under the
        // exact key `put_value` will look up for the bare PV name, so
        // `get_or_open` is a cache hit and the typed PUT reaches the
        // pinned test server.
        let out_cfg = resolver.out_cfg_for(pv_name);
        let client = PvaClient::builder()
            .server_addr(addr)
            .timeout(std::time::Duration::from_secs(3))
            .build();
        let link = std::sync::Arc::new(PvaLink::for_test_with_client(
            PvaLinkConfig::clone(&out_cfg),
            client,
        ));
        resolver.registry.insert_for_test(&out_cfg, link);

        // Drive the OUT path through the resolver's LinkSet impl.
        let scheme_name = format!("pva://{pv_name}");
        LinkSet::put_value(&resolver, scheme_name.as_bytes(), value, LinkPutOp::Plain)
            .await
            .expect("typed array OUT write must succeed");

        tokio::time::sleep(std::time::Duration::from_millis(80)).await;

        let current = pv.current().expect("PV has a current value");
        let PvField::Structure(s) = current else {
            panic!("expected structure value");
        };
        let value_field = s.get_field("value").expect("value sub-field present");
        // Compare as i64 bit patterns — a ulong[] readback is ULong,
        // a long[] readback is Long; both carry the same 64-bit word.
        let elem_as_i64 = |sv: &ScalarValue| -> i64 {
            match sv {
                ScalarValue::Long(x) => *x,
                ScalarValue::ULong(x) => *x as i64,
                other => panic!("expected a 64-bit element, got {other:?}"),
            }
        };
        let got: Vec<i64> = match value_field {
            PvField::ScalarArray(v) => v.iter().map(elem_as_i64).collect(),
            PvField::ScalarArrayTyped(t) => t.to_scalar_values().iter().map(elem_as_i64).collect(),
            other => panic!("expected an array value field, got {other:?}"),
        };
        assert_eq!(
            got, expected,
            "typed-array OUT write must land the full-width 64-bit array value"
        );
    }

    /// One `ao` per `(name, FLNK)`, loaded from `.db` text so the resolver
    /// installed before iocInit is what opens each link.
    async fn db_with_flnks(flnks: &[(&str, &str)]) -> Arc<PvDatabase> {
        let text: String = flnks
            .iter()
            .map(|(name, flnk)| {
                let flnk = flnk.replace('"', "\\\"");
                format!("record(ao, \"{name}\") {{ field(FLNK, \"{flnk}\") }}\n")
            })
            .collect();
        epics_base_rs::server::ioc_builder::IocBuilder::new()
            .register_link_type("pva", |_db| async { Vec::new() })
            .db_string(&text, &std::collections::HashMap::new())
            .unwrap()
            .build()
            .await
            .unwrap()
            .0
    }

    /// The STAT a record with `flnk` settles on, forwarding every 100 ms for
    /// 2 s to a PV no server has: long enough for the forward's link to
    /// open, so the refusal tested is the disconnected one.
    async fn forward_stat_toward_an_absent_pv(name: &str, flnk: &str) -> u16 {
        let db = db_with_flnks(&[(name, flnk)]).await;
        let _resolver = install_pvalink_resolver(&db).await;
        db.ioc_init().await;
        let mut stat = 0;
        for _ in 0..20 {
            task::sleep(std::time::Duration::from_millis(100)).await;
            let _ = db.process_record(name).await;
            stat = db.get_record(name).unwrap().read().common.stat;
        }
        stat
    }

    /// pvxs `pvaScanForward` (`pvalink_lset.cpp:677-680`) refuses a
    /// disconnected forward with LINK/INVALID "Disconn" unless the link was
    /// opened with `retry`, whose put the client holds until it connects.
    #[epics_base_rs::epics_test]
    async fn a_json_forward_is_refused_while_disconnected_only_without_retry() {
        use epics_base_rs::server::recgbl::alarm_status::LINK_ALARM;
        let held =
            forward_stat_toward_an_absent_pv("SRCJ", r#"{pva:{pv:"ZZ:ABSENT:J", retry:true}}"#)
                .await;
        assert_ne!(held, LINK_ALARM, "a retry forward is held, not refused");
        let refused =
            forward_stat_toward_an_absent_pv("SRCN", r#"{pva:{pv:"ZZ:ABSENT:N", retry:false}}"#)
                .await;
        assert_eq!(refused, LINK_ALARM, "a plain forward is refused as Disconn");
    }

    /// A JSON forward put after iocInit is opened with its own members
    /// (pvxs `pvaOpenLink` gets the link's `jlink`): no option scan ever
    /// sees it, so its `retry` must reach the handle from the link.
    #[epics_base_rs::epics_test]
    async fn a_json_forward_put_at_runtime_keeps_its_retry() {
        // `pva` registered, as a runtime `{pva:…}` put needs.
        let (db, _) = epics_base_rs::server::ioc_builder::IocBuilder::new()
            .register_link_type("pva", |_db| async { Vec::new() })
            .db_string(
                r#"record(ao, "SRCR") {}"#,
                &std::collections::HashMap::new(),
            )
            .unwrap()
            .build()
            .await
            .unwrap();
        let _resolver = install_pvalink_resolver(&db).await;
        db.ioc_init().await;
        db.put_record_field_from_ca(
            "SRCR",
            "FLNK",
            EpicsValue::String(r#"{pva:{pv:"ZZ:ABSENT:R", retry:true}}"#.into()),
        )
        .await
        .unwrap();
        let mut stat = 0;
        for _ in 0..20 {
            task::sleep(std::time::Duration::from_millis(100)).await;
            let _ = db.process_record("SRCR").await;
            stat = db.get_record("SRCR").unwrap().read().common.stat;
        }
        assert_ne!(
            stat,
            epics_base_rs::server::recgbl::alarm_status::LINK_ALARM,
            "a retry forward is held, not refused"
        );
    }

    /// Two forwards to one PV share its channel but not its `retry`: pvxs
    /// gates on the link's own `self->retry` (`pvalink_lset.cpp:677`).
    #[epics_base_rs::epics_test]
    async fn forwards_sharing_a_channel_keep_their_own_retry() {
        use epics_base_rs::server::recgbl::alarm_status::LINK_ALARM;
        let flnks = [("SRCA", true), ("SRCB", false), ("SRCC", true)].map(|(name, retry)| {
            (
                name,
                format!(r#"{{pva:{{pv:"ZZ:ABSENT:S", retry:{retry}}}}}"#),
            )
        });
        let db = db_with_flnks(&flnks.each_ref().map(|(n, f)| (*n, f.as_str()))).await;
        let _resolver = install_pvalink_resolver(&db).await;
        db.ioc_init().await;
        for _ in 0..20 {
            task::sleep(std::time::Duration::from_millis(100)).await;
            for name in ["SRCA", "SRCB", "SRCC"] {
                let _ = db.process_record(name).await;
            }
        }
        let stat = |name| db.get_record(name).unwrap().read().common.stat;
        assert_ne!(stat("SRCA"), LINK_ALARM, "SRCA retries");
        assert_eq!(stat("SRCB"), LINK_ALARM, "SRCB does not");
        assert_ne!(stat("SRCC"), LINK_ALARM, "SRCC retries");
    }

    /// The channel a JSON `field` put at runtime opens, after `rounds`
    /// processes of `rec`: no option scan sees such a link, so its members
    /// reach the lset only through the key it is addressed by.
    async fn channel_of_a_runtime_json_link(
        rec_type: &str,
        field: &str,
        pv: &str,
    ) -> Option<super::super::registry::ChannelDiag> {
        let (db, _) = epics_base_rs::server::ioc_builder::IocBuilder::new()
            .register_link_type("pva", |_db| async { Vec::new() })
            .db_string(
                &format!(r#"record({rec_type}, "RT") {{}}"#),
                &std::collections::HashMap::new(),
            )
            .unwrap()
            .build()
            .await
            .unwrap();
        let resolver = install_pvalink_resolver(&db).await;
        db.ioc_init().await;
        db.put_record_field_from_ca(
            "RT",
            field,
            EpicsValue::String(format!(r#"{{pva:{{pv:"{pv}", Q:7}}}}"#).into()),
        )
        .await
        .unwrap();
        for _ in 0..20 {
            let _ = db.process_record("RT").await;
            if let Some(c) = resolver
                .channel_diagnostics()
                .into_iter()
                .find(|c| c.pv_name == pv)
            {
                return Some(c);
            }
            task::sleep(std::time::Duration::from_millis(100)).await;
        }
        None
    }

    #[epics_base_rs::epics_test]
    async fn a_json_inp_link_put_at_runtime_opens_with_its_own_options() {
        let c = channel_of_a_runtime_json_link("ai", "INP", "ZZ:ABSENT:I")
            .await
            .expect("the INP link opened a channel");
        assert_eq!((c.direction, c.queue_size), (LinkDirection::Inp, 7));
    }

    #[epics_base_rs::epics_test]
    async fn a_json_out_link_put_at_runtime_opens_with_its_own_options() {
        let c = channel_of_a_runtime_json_link("ao", "OUT", "ZZ:ABSENT:O")
            .await
            .expect("the OUT link opened a channel");
        assert_eq!((c.direction, c.queue_size), (LinkDirection::Out, 7));
    }

    /// A JSON string can carry the identity key's separator in any field: a
    /// Str option holding it configures the link exactly as its options do,
    /// and a `pv` holding it is looked up under the channel it opened.
    #[epics_base_rs::epics_test]
    async fn a_json_link_holding_the_key_separator_keeps_its_config() {
        let db = Arc::new(PvDatabase::new());
        let resolver = install_pvalink_resolver(&db).await;
        let options = vec![
            ("Q".to_string(), JlinkValue::Int(7)),
            ("field".to_string(), JlinkValue::Str("a\u{1f}b".into())),
        ];
        let pv = "P\u{1f}V";
        let key = pvajson_identity_key(pv, &options);
        let want = PvaLinkConfig::from_jlink_options(pv, &options, LinkDirection::Out).unwrap();
        assert_eq!(*resolver.out_cfg_for(&key), want);
        let _ = resolver.opener.open(&want).await;
        assert_eq!(
            resolver.put_admission(key.as_bytes()),
            PutAdmission::Refused
        );
    }

    /// `{pva:{pv:" ",Q:7}}` put at runtime into `field` of `rec`, then a
    /// second of processing: the channels, and the record's (STAT, SEVR,
    /// AMSG, NAMSG). softIocPVX (pvxs 1.5.3), given the same link and ten
    /// processes, opens a channel named `" "` that never connects.
    async fn after_a_space_named_runtime_json_link(
        rec_type: &str,
        field: &str,
    ) -> (
        Vec<super::super::registry::ChannelDiag>,
        (
            u16,
            epics_base_rs::server::record::AlarmSeverity,
            String,
            String,
        ),
    ) {
        let (db, _) = epics_base_rs::server::ioc_builder::IocBuilder::new()
            .register_link_type("pva", |_db| async { Vec::new() })
            .db_string(
                &format!(r#"record({rec_type}, "RT") {{}}"#),
                &std::collections::HashMap::new(),
            )
            .unwrap()
            .build()
            .await
            .unwrap();
        let resolver = install_pvalink_resolver(&db).await;
        db.ioc_init().await;
        db.put_record_field_from_ca(
            "RT",
            field,
            EpicsValue::String(r#"{pva:{pv:" ",Q:7}}"#.into()),
        )
        .await
        .unwrap();
        for _ in 0..10 {
            let _ = db.process_record("RT").await;
            task::sleep(std::time::Duration::from_millis(100)).await;
        }
        let rec = db.get_record("RT").unwrap();
        let alarm = {
            let inst = rec.read();
            (
                inst.common.stat,
                inst.common.sevr,
                inst.common.amsg.to_string(),
                inst.common.namsg.to_string(),
            )
        };
        (resolver.channel_diagnostics(), alarm)
    }

    fn one_unconnected_space_channel(channels: &[super::super::registry::ChannelDiag]) {
        assert_eq!(channels.len(), 1, "{channels:?}");
        assert_eq!(channels[0].pv_name, " ");
        assert!(!channels[0].connected);
    }

    /// softIocPVX: `LINK`/`INVALID`.
    #[epics_base_rs::epics_test]
    async fn a_json_inp_link_named_a_space_opens_and_alarms() {
        let (channels, (stat, sevr, _, _)) =
            after_a_space_named_runtime_json_link("ai", "INP").await;
        one_unconnected_space_channel(&channels);
        assert_eq!(
            (stat, sevr),
            (
                epics_base_rs::server::recgbl::alarm_status::LINK_ALARM,
                epics_base_rs::server::record::AlarmSeverity::Invalid,
            )
        );
    }

    /// softIocPVX: `LINK`/`INVALID`, AMSG and NAMSG `field OUT`.
    #[epics_base_rs::epics_test]
    async fn a_json_out_link_named_a_space_opens_and_alarms() {
        let (channels, alarm) = after_a_space_named_runtime_json_link("ao", "OUT").await;
        one_unconnected_space_channel(&channels);
        assert_eq!(
            alarm,
            (
                epics_base_rs::server::recgbl::alarm_status::LINK_ALARM,
                epics_base_rs::server::record::AlarmSeverity::Invalid,
                "field OUT".to_string(),
                "field OUT".to_string(),
            )
        );
    }

    /// softIocPVX: `LINK`/`INVALID`, AMSG and NAMSG `Disconn`
    /// (`pvaScanForward`, `pvxs/ioc/pvalink_lset.cpp:685-687`).
    #[epics_base_rs::epics_test]
    async fn a_json_forward_link_named_a_space_opens() {
        let (channels, alarm) = after_a_space_named_runtime_json_link("ao", "FLNK").await;
        one_unconnected_space_channel(&channels);
        assert_eq!(
            alarm,
            (
                epics_base_rs::server::recgbl::alarm_status::LINK_ALARM,
                epics_base_rs::server::record::AlarmSeverity::Invalid,
                "Disconn".to_string(),
                "Disconn".to_string(),
            )
        );
    }
}

/// An alarmed remote is C's every-cycle `recGblSetSevrMsg(LINK_ALARM, sev,
/// snap_message)` (`pvalink_lset.cpp:424-434`), a copy into the record's
/// fixed `namsg`. Here the message is the `Arc<str>` the link latched when
/// the text arrived, so the steady cycle allocates nothing and a changed
/// remote text costs exactly the one allocation of the new text. Counted on
/// a primed IOC thread with this binary's own allocator.
#[cfg(test)]
mod alarmed_remote_cycle_alloc {
    use std::alloc::{GlobalAlloc, Layout, System};
    use std::cell::Cell;
    use std::collections::HashMap;
    use std::sync::Arc;

    use epics_base_rs::runtime::task::{MandatoryThread, StackSizeClass, ThreadPriority};
    use epics_base_rs::server::database::{ProcStack, PvDatabase};
    use epics_base_rs::server::ioc_builder::IocBuilder;
    use epics_pva_rs::pvdata::{PvField, PvStructure, ScalarValue};

    use super::{LinkDirection, PvaLink, PvaLinkConfig, PvaLinkResolver, install_pvalink_resolver};

    thread_local! {
        static COUNTING: Cell<bool> = const { Cell::new(false) };
        static ALLOCS: Cell<usize> = const { Cell::new(0) };
    }

    struct Counting;

    // SAFETY: forwards every call to `System` unchanged; the counters are
    // const-initialised thread-local `Cell`s, which never allocate.
    unsafe impl GlobalAlloc for Counting {
        unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
            if COUNTING.with(Cell::get) {
                ALLOCS.with(|n| n.set(n.get() + 1));
            }
            unsafe { System.alloc(layout) }
        }

        unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, size: usize) -> *mut u8 {
            if COUNTING.with(Cell::get) {
                ALLOCS.with(|n| n.set(n.get() + 1));
            }
            unsafe { System.realloc(ptr, layout, size) }
        }

        unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
            unsafe { System.dealloc(ptr, layout) }
        }
    }

    #[global_allocator]
    static GLOBAL: Counting = Counting;

    const DB: &str = r#"
record(ai, "AR:AI") { field(INP, "pva://AR:PV?sevr=MS") }
record(ai, "AR:JSON") { field(INP, {pva:{pv:"AR:PV", sevr:"MS"}}) }
"#;

    /// An NTScalar at MAJOR carrying `message`.
    fn alarmed(message: &str) -> PvField {
        let mut alarm = PvStructure::new("alarm_t");
        alarm
            .fields
            .push(("severity".into(), PvField::Scalar(ScalarValue::Int(2))));
        alarm.fields.push((
            "message".into(),
            PvField::Scalar(ScalarValue::String(message.into())),
        ));
        let mut root = PvStructure::new("epics:nt/NTScalar:1.0");
        root.fields
            .push(("value".into(), PvField::Scalar(ScalarValue::Double(3.0))));
        root.fields
            .push(("alarm".into(), PvField::Structure(alarm)));
        PvField::Structure(root)
    }

    /// Allocations `cycles` cycles of `rec` make on a primed scan thread,
    /// after `before` ran there uncounted.
    fn allocs(
        db: Arc<PvDatabase>,
        rec: &'static str,
        cycles: usize,
        before: impl FnOnce() + Send + 'static,
    ) -> usize {
        let (tx, rx) = std::sync::mpsc::channel();
        MandatoryThread::new("scan-1", ThreadPriority::ScanLow, StackSizeClass::Big).spawn(
            move || {
                for _ in 0..3 {
                    db.process_record_with_links_already_locked(rec, &mut ProcStack::new())
                        .unwrap();
                }
                before();
                let mut visited = ProcStack::new();
                ALLOCS.with(|n| n.set(0));
                COUNTING.with(|c| c.set(true));
                for _ in 0..cycles {
                    db.process_record_with_links_already_locked(rec, &mut visited)
                        .unwrap();
                }
                COUNTING.with(|c| c.set(false));
                tx.send(ALLOCS.with(Cell::get)).unwrap();
            },
        );
        rx.recv().unwrap()
    }

    fn alarm_of(db: &PvDatabase, rec: &str) -> (String, String) {
        let pv = |f: &str| db.get_pv(format!("{rec}.{f}")).unwrap().to_string();
        (pv("SEVR"), pv("AMSG"))
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn an_alarmed_remote_cycle_allocates_only_for_a_new_text() {
        let resolver: Arc<parking_lot::Mutex<Option<PvaLinkResolver>>> = Arc::default();
        let installed = resolver.clone();
        let (db, _) = IocBuilder::new()
            .register_link_type("pva", move |db| {
                let installed = installed.clone();
                async move {
                    *installed.lock() = Some(install_pvalink_resolver(&db).await);
                    Vec::new()
                }
            })
            .db_string(DB, &HashMap::new())
            .unwrap()
            .build()
            .await
            .unwrap();
        let cfg = PvaLinkConfig {
            monitor: true,
            ..PvaLinkConfig::defaults_for("AR:PV", LinkDirection::Inp)
        };
        let link = Arc::new(PvaLink::for_test(cfg.clone(), Some(alarmed("HIGH"))));
        let resolver = resolver.lock().clone().expect("pva link type installed");
        resolver.registry.insert_for_test(&cfg, link.clone());
        db.ioc_init().await;

        for rec in ["AR:AI", "AR:JSON"] {
            assert_eq!(allocs(db.clone(), rec, 100, || ()), 0, "{rec} steady");
            assert_eq!(alarm_of(&db, rec), ("2".into(), "HIGH".into()), "{rec}");
        }
        let changed = link.clone();
        let one = allocs(db.clone(), "AR:AI", 1, move || {
            changed.set_latest_for_test(alarmed("HIHI"));
        });
        assert_eq!(one, 1, "a new remote text is one allocation");
        assert_eq!(alarm_of(&db, "AR:AI"), ("2".into(), "HIHI".into()));
        assert_eq!(allocs(db.clone(), "AR:AI", 100, || ()), 0, "settled again");
    }
}
