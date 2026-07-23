//! Wire pvalink up to the record-link resolver in `epics-base-rs`.
//!
//! The integration plan:
//!
//! 1. `PvaLinkResolver` owns a [`PvaLinkRegistry`] (PvaLink cache); the
//!    synchronous resolver closure submits background work through the
//!    `epics_base_rs::runtime::task` spawn seam — `tokio::spawn` on the host,
//!    the callback pool on the RTEMS target where no tokio runtime exists.
//! 2. [`install_pvalink_resolver`] hooks the resolver into the database via
//!    `PvDatabase::set_external_resolver`. Records with `INP=@pva://...`
//!    will then resolve through the registry instead of returning `None`.
//! 3. INP links are pre-warmed via [`PvaLinkResolver::open`] (also exposed
//!    as the `pvxr` iocsh command) so the synchronous resolver path can
//!    return the cached monitor value without blocking on a fresh GET.
//!    Out-of-band reads still work — `block_on` will issue a GET — but
//!    pre-warmed monitors are always cheaper.
//!
//! pvxs equivalent: `ioc/pvalink.cpp` + `pvalink_channel.cpp`
//! (`pvalinkInit`, `pvalinkOpen`, `dbpvxr`).

// RTEMS-EXEC-MODEL-ALLOW(38): checked - these run and pass in the feature-ON suite.

use std::sync::Arc;

use epics_base_rs::runtime::task;
use epics_base_rs::server::database::{
    ExternalPvResolver, LinkPutOp, LinkSet, PutAdmission, PvDatabase,
};
use epics_base_rs::server::record::{JlinkValue, PVAJSON_IDENTITY_SEP, pvajson_identity_key};
use epics_base_rs::types::EpicsValue;
use epics_pva_rs::pvdata::{PvField, ScalarValue};

use super::config::{LinkDirection, PvaLinkConfig};
use super::link::{PvaLink, PvaLinkError, PvaLinkResult, ScanEvent, ScanOverrun};
use super::registry::PvaLinkRegistry;

/// Resolver wrapping a [`PvaLinkRegistry`]. Cheap to clone — every field is
/// `Arc`-backed. Background work is submitted through the
/// `epics_base_rs::runtime::task` spawn seam, so the resolver holds no tokio
/// runtime handle of its own (the seam picks tokio or the callback pool by
/// target).
#[derive(Clone)]
pub struct PvaLinkResolver {
    registry: Arc<PvaLinkRegistry>,
    /// Counter incremented on every successful link read. Used by
    /// `pvalinkrefdiff` to report "links touched since last call". Wraps
    /// at u64::MAX.
    reads: Arc<std::sync::atomic::AtomicU64>,
    /// Master enable flag. Set false via [`Self::set_enabled`] (or
    /// the `pvalink_disable` iocsh command) to make every resolve
    /// return None — useful for site-level pvalink kill switches.
    /// Mirrors pvxs `pvalink_enable` / `pvalink_disable` iocsh
    /// commands (pvalink.cpp:328).
    enabled: Arc<std::sync::atomic::AtomicBool>,
    /// Per-PV link-option overrides.
    ///
    /// The `epics-base-rs` link parser collapses `@pva://X?sevr=MS`
    /// (and the legacy `pva://X MS` suffix form) down to a bare PV
    /// name in `ParsedLink::Pva`, dropping every query option before
    /// the bridge resolver is consulted. To keep B2 (`MS`/`NMS`) and
    /// the B4 options effective on the resolver hot path, the bridge
    /// stashes the parsed [`PvaLinkConfig`] here keyed by PV name when
    /// a full link string is opened via [`Self::open_link`]; the
    /// resolver then reuses those options instead of the `NMS`/Q=4
    /// defaults. Mirrors the role of pvxs `pvaLinkConfig` carried on
    /// the `jlink` for the lifetime of the link.
    link_options: Arc<parking_lot::RwLock<std::collections::HashMap<String, PvaLinkConfig>>>,
    /// Per-PV OUT link-option overrides.
    ///
    /// Mirrors `link_options` for the OUT direction: `put_value` uses
    /// these to carry the operator's `proc`, `field`, `defer`, `retry`
    /// settings to the upstream PUT, instead of building a fresh
    /// default config on every write. Populated by [`Self::open_out_link`].
    out_link_options: Arc<parking_lot::RwLock<std::collections::HashMap<String, PvaLinkConfig>>>,
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
}

/// Identity of an INP monitor variant: the subset of the registry key
/// that distinguishes two monitors of the same upstream PV. Mirrors
/// pvxs `channels_key_t = (channelName, pvRequest)` where the pvRequest
/// encodes `pipeline` / `queueSize` (`pvxs/ioc/pvalink.h:115-120`).
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
    record: String,
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
    /// SCAN being Passive (pvalink_channel.cpp:313). True here means
    /// "skip processing when the owning record's SCAN != Passive".
    passive_only: bool,
}

impl Default for PvaLinkResolver {
    fn default() -> Self {
        Self::new()
    }
}

impl PvaLinkResolver {
    pub fn new() -> Self {
        Self {
            registry: Arc::new(PvaLinkRegistry::new()),
            reads: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            enabled: Arc::new(std::sync::atomic::AtomicBool::new(true)),
            link_options: Arc::new(parking_lot::RwLock::new(std::collections::HashMap::new())),
            out_link_options: Arc::new(parking_lot::RwLock::new(std::collections::HashMap::new())),
            db: Arc::new(parking_lot::RwLock::new(None)),
            scan_targets: Arc::new(parking_lot::RwLock::new(std::collections::HashMap::new())),
            forwarders: Arc::new(parking_lot::Mutex::new(std::collections::HashSet::new())),
            #[cfg(feature = "qsrv-core")]
            qsrv: Arc::new(parking_lot::RwLock::new(None)),
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

    /// Master enable / disable. When disabled, the resolver closure
    /// returns `None` for every lookup so dependent records see
    /// LINK/INVALID alarms but no stale cached values bleed through.
    /// Mirrors pvxs `pvalink_enable(false)` / `pvalink_disable`.
    pub fn set_enabled(&self, on: bool) {
        self.enabled.store(on, std::sync::atomic::Ordering::Relaxed);
    }

    /// Read the current enable flag.
    pub fn is_enabled(&self) -> bool {
        self.enabled.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Open / cache a link for `pv_name` in INP+monitor mode. Mirrors
    /// pvxs `pvalinkOpen` (pvalink_channel.cpp). After this returns,
    /// later calls to [`Self::resolve`] for the same name will read
    /// the cached monitor value (no async block).
    ///
    /// Honors any link options previously registered for `pv_name`
    /// via [`Self::open_link`] — otherwise pvxs defaults apply.
    pub async fn open(&self, pv_name: &str) -> PvaLinkResult<Arc<PvaLink>> {
        self.registry.get_or_open(self.inp_cfg_for(pv_name)).await
    }

    /// Open / cache a link from a full `@pva://...` link string,
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
        record: &str,
    ) -> PvaLinkResult<Arc<PvaLink>> {
        self.open_link_inner(link_string, Some(record.to_string()))
            .await
    }

    async fn open_link_inner(
        &self,
        link_string: &str,
        record: Option<String>,
    ) -> PvaLinkResult<Arc<PvaLink>> {
        let cfg = PvaLinkConfig::parse(link_string, LinkDirection::Inp)?;
        // Convenience-URI path: key the per-link options by the full
        // scheme-stripped link string (including any `?query`), so two
        // links to the same PV with different options each keep their
        // own entry.
        let options_key = strip_scheme(link_string).unwrap_or(link_string).to_string();
        self.open_inp_cfg(cfg, options_key, record).await
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
        record: &str,
    ) -> PvaLinkResult<Arc<PvaLink>> {
        let cfg = PvaLinkConfig::from_jlink_options(pv, options, LinkDirection::Inp)?;
        let options_key = pvajson_identity_key(pv, options);
        self.open_inp_cfg(cfg, options_key, Some(record.to_string()))
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
        record: Option<String>,
    ) -> PvaLinkResult<Arc<PvaLink>> {
        // An INP link that is meaningful for the resolver must keep a
        // monitor open; force it on (pvxs treats `proc=CP/CPP` and the
        // resolver path as monitored).
        let cfg = PvaLinkConfig {
            monitor: true,
            ..cfg
        };
        // B4 `local`: a `local`-flagged link must resolve a PV served by
        // *this* IOC. Reject up-front so the operator gets a clear error
        // instead of a silent remote resolution. The identical gate runs
        // on the OUT open and lazy-write paths — see
        // [`Self::check_local_admission`].
        self.check_local_admission(&cfg).await?;
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
        // Record the owning record on this channel's back-index (every
        // INP link, not just CP/CPP scan targets) so `dbpvar` can filter
        // channels by record-name glob, matching pvxs's
        // `pvaLinkChannel::links` (`pvalink.cpp:213-243`).
        if let Some(rec) = &record {
            self.registry.attach_record(&cfg, rec);
        }
        // B3: register the scan-on-update target before opening so the
        // forwarder spawned below already sees it. Keyed by the monitor
        // variant identity, NOT bare PV name, so a `Q=1` record's scan
        // targets are not merged with a `Q=64` sibling's
        // (pvxs keys links onto their channel
        // by `(channelName, pvRequest)`, `pvxs/ioc/pvalink.h:115-120`).
        let monitor_key = MonitorKey::from_config(&cfg);
        // Track the newly-attached scan target so an attach-time scan
        // can fire below once the (possibly reused) channel is known
        // connected. `(record, passive_only)`.
        let attach_target: Option<(String, bool)> = match record {
            Some(rec) if cfg.scan_on_update => {
                self.scan_targets
                    .write()
                    .entry(monitor_key.clone())
                    .or_default()
                    .records
                    .push(ScanTarget {
                        record: rec.clone(),
                        monorder: cfg.monorder,
                        atomic: cfg.atomic,
                        passive_only: cfg.scan_on_passive,
                    });
                Some((rec, cfg.scan_on_passive))
            }
            _ => None,
        };
        let link = self.registry.get_or_open(cfg).await?;
        self.spawn_notify_forwarder(monitor_key, &link);
        // Attach-time scan (pvxs `pvalink_lset.cpp:148-167`,
        // `scanOnce(plink->precord)`): when the shared channel is
        // ALREADY connected at attach, fire one immediate scan of just
        // the newly attached record. This is observably needed on the
        // reuse-of-an-already-connected-monitor path —
        // `spawn_notify_forwarder` returned early without wiring a
        // fresh first-event scan for this record, so without this the
        // record stays unprocessed until the next upstream update. A
        // freshly-opened monitor variant is not yet connected here and
        // gets its first scan from the monitor task's `on_event`.
        if let Some((rec, passive_only)) = attach_target {
            if link.is_connected() {
                self.scan_attached_record(&rec, passive_only).await;
            }
        }
        Ok(link)
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
        task::spawn(run_notify_forwarder(
            key,
            rx,
            scan_targets,
            db,
            scan_overrun,
        ));
    }

    /// Fire one immediate scan of a single newly-attached scan target —
    /// the attach-time scan pvxs runs when the shared channel is already
    /// connected at link-open (`pvalink_lset.cpp:148-167`). Applies the
    /// same CPP passive gate + PACT pre-check as the steady-state
    /// forwarder, then processes the one record through the
    /// gate-acquiring entry. There is no atomic group and no epoch is
    /// held at attach time, so the link's `atomic` flag does not change
    /// the entry here — it governs only the multi-record group locking
    /// inside [`scan_once`].
    async fn scan_attached_record(&self, record: &str, passive_only: bool) {
        let Some(db_handle) = self.db.read().clone() else {
            return;
        };
        if scan_target_should_process(&db_handle, record, passive_only) {
            let mut visited = std::collections::HashSet::new();
            let _ = db_handle
                .process_record_with_links(record, &mut visited, 0)
                .await;
        }
    }

    /// Shared `local`-admission gate, applied before every pvalink
    /// `registry.get_or_open` on both the INP and OUT paths.
    ///
    /// pvxs applies `pvaLinkConfig::local` inside `pvaOpenLink()`
    /// (`ioc/pvalink_lset.cpp:69-74`), the single open function used for
    /// every link direction: if `local` is set and `dbChannelTest()`
    /// cannot find the channel in this IOC, the link set is cleared
    /// (`plink->lset = NULL`) and the link never opens, reads, or writes a
    /// channel. This helper is the single owner of that decision so a
    /// `local=true` link can never open — or reuse a non-local sibling's
    /// already-open — remote channel: every caller MUST invoke it before
    /// `registry.get_or_open`. A non-`local` config passes unconditionally.
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
    async fn check_local_admission(&self, cfg: &PvaLinkConfig) -> PvaLinkResult<()> {
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
                is_local = provider.hosts_pv(pv_name).await;
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
    /// (`dbChannelTest.c:183-186`).
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
        if db.find_pv(&record_path).await.is_some() {
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

    /// Build the INP config for a link, applying any options registered
    /// via [`Self::open_link`]. `full` may be a bare PV name or a
    /// query-bearing string (`PV?field=F&proc=CPP`). Lookup order:
    /// full string first, then bare PV name, then pvxs defaults.
    ///
    /// keying by full string ensures two links to the same PV
    /// with different options each return their own config
    /// (`pvxs/ioc/pvalink.h:65` per-link `pvaLinkConfig`).
    fn inp_cfg_for(&self, full: &str) -> PvaLinkConfig {
        let opts = self.link_options.read();
        if let Some(cfg) = opts.get(full) {
            return PvaLinkConfig {
                monitor: true,
                ..cfg.clone()
            };
        }
        let bare = link_pv_name(full);
        if bare != full {
            if let Some(cfg) = opts.get(bare) {
                return PvaLinkConfig {
                    monitor: true,
                    ..cfg.clone()
                };
            }
        }
        default_inp_cfg(bare)
    }

    /// Build the OUT config for a link. `full` may be bare or
    /// query-bearing; lookup order matches `inp_cfg_for`.
    ///
    /// per-link config isolation for OUT links.
    fn out_cfg_for(&self, full: &str) -> PvaLinkConfig {
        let opts = self.out_link_options.read();
        if let Some(cfg) = opts.get(full) {
            return cfg.clone();
        }
        let bare = link_pv_name(full);
        if bare != full {
            if let Some(cfg) = opts.get(bare) {
                return cfg.clone();
            }
        }
        PvaLinkConfig::defaults_for(bare, LinkDirection::Out)
    }

    /// Open / cache an OUT link from a full `@pva://...` link string,
    /// parsing and retaining its options (`proc`, `field`, `defer`,
    /// `retry`, ...). The parsed [`PvaLinkConfig`] is stashed under the
    /// bare PV name so the `put_value` resolver hot-path picks up the
    /// operator's proc / field / defer settings on every write.
    ///
    /// Mirrors [`Self::open_link`] for the OUT direction. pvxs equivalent:
    /// `pvaLinkConfig` carried on the `jlink` (pvalink_jlif.cpp).
    pub async fn open_out_link(
        &self,
        link_string: &str,
        record: Option<&str>,
    ) -> PvaLinkResult<Arc<PvaLink>> {
        let cfg = PvaLinkConfig::parse(link_string, LinkDirection::Out)?;
        // key by full link string (same rationale as open_link_inner).
        let options_key = strip_scheme(link_string).unwrap_or(link_string).to_string();
        self.open_out_cfg(cfg, options_key, record).await
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
        record: Option<&str>,
    ) -> PvaLinkResult<Arc<PvaLink>> {
        let cfg = PvaLinkConfig::from_jlink_options(pv, options, LinkDirection::Out)?;
        let options_key = pvajson_identity_key(pv, options);
        self.open_out_cfg(cfg, options_key, record).await
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
        record: Option<&str>,
    ) -> PvaLinkResult<Arc<PvaLink>> {
        // `local`-admission gate before any registration or open, so a
        // rejected `local=true` OUT link leaves no stale option entry and
        // never opens — or reuses a sibling's already-open — remote
        // channel (pvxs `pvaOpenLink`, direction-agnostic).
        self.check_local_admission(&cfg).await?;
        self.out_link_options
            .write()
            .insert(options_key, cfg.clone());
        // Back-index the owning record for `dbpvar`'s record-name glob
        // filter (pvxs `pvaLinkChannel::links`).
        if let Some(rec) = record {
            self.registry.attach_record(&cfg, rec);
        }
        self.registry.get_or_open(cfg).await
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
    /// `pvaGetAlarmMsg`'s severity output (pvalink_lset.cpp:544).
    pub fn link_alarm_severity(&self, pv_name: &str) -> Option<i32> {
        // apply the caller's own parsed `sevr` mode and resolve the
        // caller's own monitor variant. Pre-fix this called
        // `link_alarm_severity()` on whichever cached INP link
        // `try_get_any` returned first for the bare PV — that link's
        // `config.sevr` and monitor variant belong to an arbitrary other
        // caller. pvxs `pvaLinkConfig` is per-link and the channel is
        // keyed by `(channelName, pvRequest)`
        // (`pvxs/ioc/pvalink.h:65,115-120`).
        let full = strip_scheme(pv_name)?;
        let bare = link_pv_name(full);
        if full != bare {
            lazy_register_inp_opts(&self.link_options, full);
        }
        let cfg = self.inp_cfg_for(full);
        let sevr = cfg.sevr;
        self.registry
            .try_get_inp(bare, cfg.pipeline, cfg.queue_size)?
            .link_alarm_severity_with(&cfg.field, sevr)
    }

    /// Wait until the link for `pv_name` has received at least one
    /// monitor event (i.e., the cached value is populated). Returns
    /// `false` on timeout. Mirrors pvxs
    /// `testqsrvWaitForLinkConnected` (pvalink.cpp:131) — the
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

    /// Build the [`ExternalPvResolver`] closure that the database
    /// expects.
    ///
    /// Cache-only, like [`LinkSet::get_cached_value`] and for the same
    /// reason: this closure runs on the record-processing thread with the
    /// record's advisory gate held. C `dbCaGetLink` reads `pca->pgetNative`
    /// under `pca->lock` and returns -1 while the link is down
    /// (`dbCa.c:448-535`, `:459-464`); it never waits for the wire. A miss
    /// therefore stages the open on the pvalink runtime — C `dbCaAddLink`'s
    /// `addAction(pca, CA_CONNECT)` (`dbCa.c:735-800`) — and reports `None`
    /// for this cycle, so the reading record takes LINK/INVALID until the
    /// monitor cache is warm.
    pub fn build_resolver(self) -> ExternalPvResolver {
        let resolver = self;
        Arc::new(move |name: &str| {
            if !resolver.is_enabled() {
                return None;
            }
            // Strip optional pva:// prefix — the resolver receives the
            // bare PV name in some link forms but the prefixed form in
            // others. `ca://` is handled by libca, not pvalink — reject.
            let full = match name.strip_prefix("pva://") {
                Some(stripped) => stripped,
                None => {
                    if name.starts_with("ca://") {
                        return None;
                    }
                    name
                }
            };
            // strip query string; lazily register per-link
            // options; get per-link config before the fast path so the
            // field selector is available for `try_read_cached_with_field`.
            let bare = link_pv_name(full);
            if full != bare {
                lazy_register_inp_opts(&resolver.link_options, full);
            }
            // cfg carries the per-link field (among other opts).
            let cfg = resolver.inp_cfg_for(full);

            // Cache hit: a previously-opened link with a cached monitor
            // value. `try_get_inp` resolves THIS caller's monitor variant
            // (`pipeline` / `Q`), so a `Q=1` record reads from its own
            // monitor, not a `Q=64` sibling sharing the PV name.
            // `try_read_cached_with_field` then applies the per-link field
            // selector.
            if let Some(link) = resolver
                .registry
                .try_get_inp(bare, cfg.pipeline, cfg.queue_size)
            {
                if let Some(value) = link.try_read_cached_with_field(&cfg.field) {
                    resolver
                        .reads
                        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    return pvfield_to_epics_value(&value);
                }
                // Open but the first monitor event has not landed. C's
                // `!pca->gotInNative` arm returns without a value
                // (`dbCa.c:465-470`).
                return None;
            }

            // Never opened. Stage the open off the record thread and refuse
            // this cycle. B4: `inp_cfg_for` so a link registered via
            // `open_link` keeps its options (`sevr`, `Q`, `pipeline`,
            // `monorder`); `default_inp_cfg` would discard them.
            let registry = resolver.registry.clone();
            task::spawn(async move {
                let _ = registry.get_or_open(cfg).await;
            });
            None
        })
    }
}

/// Install a [`PvaLinkResolver`] on `db`. Returns the resolver so the
/// caller can pre-open links and query stats (`db_pvxr` /
/// `pvalinkrefdiff` iocsh commands lean on this).
///
/// Registers the resolver under the `"pva"` lset scheme *and*
/// installs the legacy [`ExternalPvResolver`] closure so callers
/// using either dispatch path work transparently.
///
/// also pre-registers pvalink options and CP/CPP scan targets
/// for every loaded DB record that carries a JSON-object pvalink with
/// options. pvxs equivalent: options live on the `jlink` struct for
/// the lifetime of the link (pvalink_jlif.cpp).
pub async fn install_pvalink_resolver(db: &Arc<PvDatabase>) -> PvaLinkResolver {
    let resolver = PvaLinkResolver::new();
    // B3: give the resolver the DB handle so the scan-on-update
    // forwarder can process owning records.
    resolver.attach_database((**db).clone());
    db.set_external_resolver(resolver.clone().build_resolver())
        .await;
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
    // (`dbLink.c:107-111`) *before* any `dbfType` discrimination, `pva_lset`
    // supplies the `DBF_FWDLINK` entry point `pvaScanForward`
    // (`pvxs/ioc/pvalink_lset.cpp:680-694`), and pvxs' own test helper accepts
    // `DBF_FWDLINK` as a pvalink field (`pvxs/ioc/pvalink.cpp:117-127`).
    //
    // An `FLNK` takes the non-`OUT` arm below on purpose even though a pvxs
    // forward link ends in a Put: [`PvaLinkResolver::scan_forward`] resolves
    // the target through `inp_cfg_for` / `try_get_inp`, i.e. the INP config and
    // registry entry this arm creates (pvxs' `pvaLinkChannel` always monitors).
    // Routing `FLNK` to `open_out_link` would register its options under the
    // OUT map, where the forward dispatch would never find them.
    for record_name in db.all_record_names().await {
        for (field_name, _raw, parsed) in db.record_link_fields(&record_name) {
            match &parsed {
                // Convenience-URI / legacy-suffix form: the verbatim
                // channel-name string may carry options in the
                // `?key=value` query form OR the legacy whitespace suffix
                // form (`TARGET MS`, `TARGET CPP`). `link_pv_name(s) == s`
                // is true only for a truly bare PV (no query, no
                // modifiers), which needs no pre-registration; anything
                // else is parsed and wired so its `field`/`sevr`/`proc`
                // and any `CP`/`CPP` scan target are effective before the
                // first read.
                ParsedLink::Pva(s) => {
                    if link_pv_name(s) == s {
                        continue;
                    }
                    let link_str = format!("pva://{s}");
                    if field_name == "OUT" {
                        let _ = resolver.open_out_link(&link_str, Some(&record_name)).await;
                    } else {
                        let _ = resolver.open_link_for_record(&link_str, &record_name).await;
                    }
                }
                // pvxs-parity JSON longhand `{pva:{pv,…}}`: the options
                // are structured JLink members, read directly without a
                // `?key=value` query round-trip.
                // Always pre-registered — the longhand variant exists
                // precisely to carry options (a pv-only longhand yields a
                // plain `Pva`), so there is no bare-PV early-out here.
                ParsedLink::PvaJson(j) => {
                    if field_name == "OUT" {
                        let _ = resolver
                            .open_json_out_link(&j.pv, &j.options, Some(&record_name))
                            .await;
                    } else {
                        let _ = resolver
                            .open_json_link_for_record(&j.pv, &j.options, &record_name)
                            .await;
                    }
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
    // `dbEvent.c:808-826`) — each armed flag owes one more scan with the
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
/// (`pvalink_channel.cpp:313-323`). Returns `true` if the record should
/// be processed now, `false` if it was gated out:
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
/// This is the single owner of the scan-time `rpro` set, shared by the
/// steady-state forwarder ([`scan_once`]) and the attach-time scan
/// ([`PvaLinkResolver::scan_attached_record`]). The write guard is
/// released before returning so the caller's process call can take its
/// own locks. A missing record returns `false`.
fn scan_target_should_process(db_handle: &PvDatabase, record: &str, passive_only: bool) -> bool {
    let Some(rec) = db_handle.get_record(record) else {
        return false;
    };
    let mut tg = rec.write();
    if passive_only && tg.common.scan != epics_base_rs::server::record::ScanType::Passive {
        return false;
    }
    if tg.is_processing() {
        tg.common.rpro = 1;
        return false;
    }
    true
}

async fn scan_once(
    monitor_key: &MonitorKey,
    scan_targets: &ScanTargetMap,
    db: &Arc<parking_lot::RwLock<Option<PvDatabase>>>,
) {
    // Snapshot the fan-out, then order it: atomic group first,
    // then non-atomic; `monorder` within each group.
    let mut targets: Vec<(String, i32, bool, bool)> = match scan_targets.read().get(monitor_key) {
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
    // across the whole atomic scan (`pvxs/ioc/pvalink_channel.cpp:386`
    // and `:422`). Acquire the database-level multi-record epoch
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
    let atomic_records: Vec<String> = targets
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
        // held (`doc/rtems-priority-locks-design.md` §1.1 H7, §5 step 6).
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
            // CPP passive gate + PACT pre-check (pvxs `ScanTrack::scan`);
            // see `scan_target_should_process`. The target write lock is
            // released before the process call below, which takes its own
            // per-record locks.
            if !scan_target_should_process(&db_handle, record, *passive_only) {
                continue;
            }
            // B3: process WITH links so the CP/CPP-driven scan fans
            // out via INP/OUT/FLNK — a pvalink feeding a calc record
            // must propagate to the calc's FLNK chain. Bare
            // `process_record` runs only `process_local` and would
            // drop the chain. Fresh `visited` set + depth 0: this is
            // the foreign-caller entry, like the scan loop and FLNK
            // dispatch.
            //
            // An atomic target runs while the epoch (`lock_records` over
            // the atomic member set) is still held — its advisory write
            // gate is already owned by this transaction. The gate is not
            // reentrant, so an atomic target MUST process through the
            // `_already_locked` entry; processing it via the
            // gate-acquiring `process_record_with_links` would dead-lock
            // the epoch against itself.
            let mut visited = std::collections::HashSet::new();
            let _ = db_handle.process_record_with_links_already_locked(record, &mut visited, 0);
        }
    }

    // Non-atomic phase — reached only after the epoch above went out of
    // scope, so each of these is a genuine fresh foreign entry that takes
    // its own per-record gate, exactly as pvxs gives each non-atomic
    // target its own `DBLocker`.
    for (record, _order, atomic, passive_only) in &targets {
        if *atomic {
            continue;
        }
        if !scan_target_should_process(&db_handle, record, *passive_only) {
            continue;
        }
        let mut visited = std::collections::HashSet::new();
        let _ = db_handle
            .process_record_with_links(record, &mut visited, 0)
            .await;
    }
}

#[epics_base_rs::async_trait]
impl LinkSet for PvaLinkResolver {
    fn is_connected(&self, name: &str) -> bool {
        // A link that has not been opened reports "not connected"; the
        // resolver hot path or `pvxr` opens it lazily.
        let Some(full) = strip_scheme(name) else {
            return false;
        };
        let bare = link_pv_name(full);
        // resolve THIS link's monitor variant. `link_names()` hands back
        // one identity string per distinct `(pv, pipeline, Q)` variant,
        // so the wait confirms each variant's own monitor connected, not
        // just the first to share the PV name.
        if full != bare {
            lazy_register_inp_opts(&self.link_options, full);
        }
        let cfg = self.inp_cfg_for(full);
        match self
            .registry
            .try_get_inp(bare, cfg.pipeline, cfg.queue_size)
        {
            Some(link) => link.is_connected(),
            None => false,
        }
    }

    async fn get_value(&self, name: &str) -> Option<EpicsValue> {
        if !self.is_enabled() {
            return None;
        }
        let full = strip_scheme(name)?;
        let bare = link_pv_name(full);
        // lazily register per-link options from query
        // string; get per-link config for field selector.
        if full != bare {
            lazy_register_inp_opts(&self.link_options, full);
        }
        let cfg = self.inp_cfg_for(full);

        // Fast path: cached monitor value, no async runtime touch.
        // resolve THIS caller's monitor variant, then apply the per-link
        // field selector.
        if let Some(link) = self
            .registry
            .try_get_inp(bare, cfg.pipeline, cfg.queue_size)
            && let Some(value) = link.try_read_cached_with_field(&cfg.field)
        {
            self.reads
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            return pvfield_to_epics_value(&value);
        }

        // Slow path: open the link / fall back to a fresh GET.
        // B4: use `inp_cfg_for` so a link registered via `open_link`
        // keeps its options (`sevr`, `Q`, `pipeline`, `monorder`);
        // `default_inp_cfg` would discard them.
        let field = cfg.field.clone();
        let value = async {
            let link = self.registry.get_or_open(cfg).await.ok()?;
            link.read_with_field(&field).await.ok()
        }
        .await?;
        self.reads
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        pvfield_to_epics_value(&value)
    }

    /// C `dbCaGetLink` (`dbCa.c:448-535`) — the monitor-fed cache only.
    /// This is [`Self::get_value`]'s fast path (`integration.rs:1155-1166`)
    /// without the `get_or_open` + `read_with_field` slow path, so the
    /// record-processing read cannot suspend on a PVA connect or GET round
    /// trip. pvxs reads the same way: `pvaGetValue` serves the monitor
    /// snapshot the channel task refreshed (`pvalink_lset.cpp:199-236`).
    fn get_cached_value(&self, name: &str) -> Option<EpicsValue> {
        if !self.is_enabled() {
            return None;
        }
        let full = strip_scheme(name)?;
        let bare = link_pv_name(full);
        if full != bare {
            lazy_register_inp_opts(&self.link_options, full);
        }
        let cfg = self.inp_cfg_for(full);
        let link = self
            .registry
            .try_get_inp(bare, cfg.pipeline, cfg.queue_size)?;
        let value = link.try_read_cached_with_field(&cfg.field)?;
        self.reads
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        pvfield_to_epics_value(&value)
    }

    /// C `dbCaAddLink`'s `CA_CONNECT` (`dbCa.c:735-800`): open the monitor so
    /// later cached reads have something to serve. Runs on the database's
    /// link work owner, never on a record-processing thread.
    async fn connect_link(&self, name: &str) {
        if !self.is_enabled() {
            return;
        }
        let Some(full) = strip_scheme(name) else {
            return;
        };
        let bare = link_pv_name(full);
        if full != bare {
            lazy_register_inp_opts(&self.link_options, full);
        }
        let _ = self.registry.get_or_open(self.inp_cfg_for(full)).await;
    }

    /// C `dbCaPutLinkCallback`'s `if (!pca->isConnected || !pca->hasWriteAccess)
    /// return -1;` (`dbCa.c:558-561`), answered from cached state only — the
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
    /// (`pvalink_channel.cpp:342-363`), and gates the write on
    /// `valid() = connected && root` (`pvalink_lset.cpp:609`).
    ///
    /// A link never opened reports `Unopened` so the write is still staged
    /// and `put_value`'s lazy `get_or_open` performs the open; pvxs opens at
    /// link-init instead, so it never sees this state.
    fn put_admission(&self, name: &str) -> PutAdmission {
        if !self.is_enabled() {
            // `put_value` would reject it — refuse at the gate so the owning
            // record alarms in this cycle, as a `-1` from C does.
            return PutAdmission::Disconnected;
        }
        let Some(full) = strip_scheme(name) else {
            // A `ca://` name reaching the pva lset: `put_value` rejects it.
            return PutAdmission::Disconnected;
        };
        let bare = link_pv_name(full);
        if full != bare {
            lazy_register_out_opts(&self.out_link_options, full);
        }
        let cfg = self.out_cfg_for(full);
        // pvxs gates the write on `if(!self->retry && !self->valid())`
        // (`pvalink_lset.cpp:609`): a `retry` link deliberately skips the
        // connection gate and queues for replay on reconnect. Refusing it
        // here would delete that whole mechanism, so a retry link is always
        // admitted and `flush_scratch` owns its disconnect handling.
        if cfg.retry {
            return PutAdmission::Connected;
        }
        match self
            .registry
            .try_get_out(bare, cfg.pipeline, cfg.queue_size)
        {
            None => PutAdmission::Unopened,
            Some(link) if link.is_connected() => PutAdmission::Connected,
            Some(_) => PutAdmission::Disconnected,
        }
    }

    async fn put_value(&self, name: &str, value: EpicsValue, op: LinkPutOp) -> Result<(), String> {
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
            format!("pvalink rejects ca:// scheme: {name} (use the CA-link path instead)")
        })?;
        let bare = link_pv_name(full);
        // lazily register per-link OUT options from query
        // string; pass full string to out_cfg_for for per-link config.
        if full != bare {
            lazy_register_out_opts(&self.out_link_options, full);
        }
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
            // `local`-admission gate on the lazy write path: a
            // `local=true` OUT link must never open — or reuse a
            // sibling's already-open — remote channel. Checked before
            // `get_or_open` so even a cache hit is gated (pvxs
            // `pvaOpenLink`, direction-agnostic).
            self.check_local_admission(&cfg)
                .await
                .map_err(|e| e.to_string())?;
            // Resolve the SHARED channel owner for this PV (the
            // registry no longer keys on the per-link OUT options),
            // then stage THIS link's write with its own
            // `field`/`proc`/`defer`/`retry` so sibling fields
            // coalesce into one upstream PUT.
            let link = self
                .registry
                .get_or_open(cfg.clone())
                .await
                .map_err(|e| e.to_string())?;
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

    /// C `dbCa.c:643-648` `scanForward`: the FWD link is a
    /// `dbCaPutLink(plink, DBR_SHORT, &fwdLinkValue, 1)`, i.e. it stages a
    /// `CA_WRITE_NATIVE` action and **returns** — the `pvprocess` round trip
    /// is the `dbCaTask`'s work, never the record thread's. The gate before
    /// staging is C's `if (!pca->isConnected) return -1` (`dbCa.c:558-561`),
    /// which pvxs surfaces as `pvaScanForward`'s `is_connected` check
    /// (`pvalink_lset.cpp:677`); a link this resolver has never opened is
    /// staged for open here and refused for this cycle, exactly as a cache
    /// miss on the value-read path is.
    fn scan_forward(&self, name: &str) -> Result<(), String> {
        if !self.is_enabled() {
            return Err("pvalink disabled".into());
        }
        let full = strip_scheme(name).ok_or_else(|| {
            format!("pvalink rejects ca:// scheme: {name} (use the CA-link path instead)")
        })?;
        let bare = link_pv_name(full);
        // lazily register per-link options from the query string; a
        // forward link shares the same monitor-backed channel a value
        // INP link would open (pvxs's `pvaLinkChannel` always monitors —
        // `pvalink_channel.cpp`), which is what `is_connected` checks.
        if full != bare {
            lazy_register_inp_opts(&self.link_options, full);
        }
        let cfg = self.inp_cfg_for(full);
        let Some(link) = self
            .registry
            .try_get_inp(bare, cfg.pipeline, cfg.queue_size)
        else {
            // Never opened. Stage the open off the record thread (C's
            // `CA_CONNECT`) and refuse this cycle.
            let registry = self.registry.clone();
            task::spawn(async move {
                let _ = registry.get_or_open(cfg).await;
            });
            return Err(format!("pvalink forward link '{name}' is not open yet"));
        };
        if !link.is_connected() {
            return Err(format!("pvalink forward link '{name}' is not connected"));
        }
        // Staged, signalled, returned — `dbCa.c:622-624`.
        task::spawn(async move {
            if let Err(e) = link.scan_forward().await {
                eprintln!("pvalink forward link scan failed: {e}");
            }
        });
        Ok(())
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

    fn alarm_message(&self, name: &str) -> Option<String> {
        // parse the caller's full link config and apply the
        // caller's own `sevr` mode — `get_or_open` may return a
        // previously cached INP link whose `config.sevr` belongs to a
        // different caller, so `default_inp_cfg(bare)` would discard
        // this caller's `?sevr=` option. pvxs `pvaLinkConfig::sevr`
        // is per-link (`pvxs/ioc/pvalink.h:65`).
        let full = strip_scheme(name)?;
        let bare = link_pv_name(full);
        if full != bare {
            lazy_register_inp_opts(&self.link_options, full);
        }
        let cfg = self.inp_cfg_for(full);
        let sevr = cfg.sevr;
        let field = cfg.field.clone();
        // Cache-only: C's `getAttributes` family reads `pca->...` and never
        // creates a channel (`dbCa.c:662-704`); pvxs's metadata getters read
        // the cached NT value under the channel lock
        // (`pvalink_lset.cpp:199-254`). The open is staged by the value-read
        // path / `connect_link` on the link work owner, never here — this runs
        // on the record thread inside the record's advisory write gate.
        let link = self
            .registry
            .try_get_inp(bare, cfg.pipeline, cfg.queue_size)?;
        link.alarm_message_with(&field, sevr)
    }

    fn alarm_severity(&self, name: &str) -> Option<i32> {
        // B2: surface the gated link-alarm severity so the owning
        // record's `LINK_ALARM` actually reflects the remote PV's
        // alarm. The `MS`/`NMS`/`MSI` gate is applied here.
        //
        // apply the caller's own `sevr` mode (parsed from the
        // caller's full link config), not the cached link's shared
        // `config.sevr`.
        let full = strip_scheme(name)?;
        let bare = link_pv_name(full);
        if full != bare {
            lazy_register_inp_opts(&self.link_options, full);
        }
        let cfg = self.inp_cfg_for(full);
        let sevr = cfg.sevr;
        let field = cfg.field.clone();
        // Cache-only: C's `getAttributes` family reads `pca->...` and never
        // creates a channel (`dbCa.c:662-704`); pvxs's metadata getters read
        // the cached NT value under the channel lock
        // (`pvalink_lset.cpp:199-254`). The open is staged by the value-read
        // path / `connect_link` on the link work owner, never here — this runs
        // on the record thread inside the record's advisory write gate.
        let link = self
            .registry
            .try_get_inp(bare, cfg.pipeline, cfg.queue_size)?;
        link.link_alarm_severity_with(&field, sevr)
    }

    fn remote_alarm(&self, name: &str) -> Option<epics_base_rs::server::database::RemoteAlarm> {
        // Ungated remote alarm snapshot for DB-link inspection
        // (`dbGetAlarm`/`dbGetAlarmMsg`). Unlike `alarm_severity`, this
        // applies NO `sevr` gate — a default `NMS` link still reports
        // its remote severity here. pvxs `pvaGetAlarmMsg`
        // (`pvalink_lset.cpp:542-575`) reads the snapshot directly. The
        // per-link `field` still selects the metadata root, matching the
        // value/alarm getters.
        let full = strip_scheme(name)?;
        let bare = link_pv_name(full);
        if full != bare {
            lazy_register_inp_opts(&self.link_options, full);
        }
        let cfg = self.inp_cfg_for(full);
        let field = cfg.field.clone();
        // Cache-only: C's `getAttributes` family reads `pca->...` and never
        // creates a channel (`dbCa.c:662-704`); pvxs's metadata getters read
        // the cached NT value under the channel lock
        // (`pvalink_lset.cpp:199-254`). The open is staged by the value-read
        // path / `connect_link` on the link work owner, never here — this runs
        // on the record thread inside the record's advisory write gate.
        let link = self
            .registry
            .try_get_inp(bare, cfg.pipeline, cfg.queue_size)?;
        link.remote_alarm_snapshot(&field)
    }

    fn time_stamp(&self, name: &str) -> Option<(i64, i32, u64)> {
        let full = strip_scheme(name)?;
        let bare = link_pv_name(full);
        if full != bare {
            lazy_register_inp_opts(&self.link_options, full);
        }
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
        let field = cfg.field.clone();
        // Cache-only: C's `getAttributes` family reads `pca->...` and never
        // creates a channel (`dbCa.c:662-704`); pvxs's metadata getters read
        // the cached NT value under the channel lock
        // (`pvalink_lset.cpp:199-254`). The open is staged by the value-read
        // path / `connect_link` on the link work owner, never here — this runs
        // on the record thread inside the record's advisory write gate.
        let link = self
            .registry
            .try_get_inp(bare, cfg.pipeline, cfg.queue_size)?;
        // only adopt the upstream timestamp when this caller's
        // link was configured with `time=true`. pvxs
        // `pvalink_lset.cpp:427` copies the latched remote NT
        // timestamp into the owning record only when this flag is
        // set; otherwise the owning record keeps its locally-stamped
        // processing time.
        if !want_time {
            return None;
        }
        link.time_stamp(&field)
    }

    fn link_metadata(&self, name: &str) -> Option<epics_base_rs::server::database::LinkMetadata> {
        // surface the remote display/control/valueAlarm
        // metadata, DBF type and element count through the DB link
        // API, mirroring the pvxs pvalink lset metadata getter set
        // installed at `pvxs/ioc/pvalink_lset.cpp:700`. Reads the
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
        let bare = link_pv_name(full);
        if full != bare {
            lazy_register_inp_opts(&self.link_options, full);
        }
        let cfg = self.inp_cfg_for(full);
        let field = cfg.field.clone();
        // Cache-only: C's `getAttributes` family reads `pca->...` and never
        // creates a channel (`dbCa.c:662-704`); pvxs's metadata getters read
        // the cached NT value under the channel lock
        // (`pvalink_lset.cpp:199-254`). The open is staged by the value-read
        // path / `connect_link` on the link work owner, never here — this runs
        // on the record thread inside the record's advisory write gate.
        let link = self
            .registry
            .try_get_inp(bare, cfg.pipeline, cfg.queue_size)?;
        link.link_metadata_with(&field)
    }

    fn link_names(&self) -> Vec<String> {
        // The [`LinkSet::link_names`] enumeration contract: surface one
        // identity per opened INP monitor variant, for link introspection.
        // NOT consumed by the iocInit external-link wait — that wait is
        // CA-facility only (`PvDatabase::external_link_targets` looks up
        // the `"ca"` set alone), and pvalink never blocks iocInit (pvxs
        // parity). A `pva://` monitor connects in the background.
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
                    pv
                } else {
                    format!("{pv}?pipeline={pipeline}&Q={qsize}")
                }
            })
            .collect()
    }
}

/// Strip the `pva://` scheme prefix the bridge sometimes prepends.
/// Pvalink only handles PVA — `ca://` is the libca scheme and is
/// dispatched by the CA-link path elsewhere, so an explicit `ca://`
/// here returns `None` so the caller can short-circuit. Names with
/// no scheme are passed through.
///
/// NOTE: the returned string may still contain a `?` query part when
/// the link was parsed from a JSON object with extra options.
/// Call `strip_query` on the result before using it as a registry key
/// or a PV name lookup.
fn strip_scheme(name: &str) -> Option<&str> {
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
/// trailing modifiers into the per-link config. The identity-key
/// separator is a control byte that cannot occur in a PV name or a URI,
/// so splitting on it is unambiguous. This collapses all option
/// representations to one PV identity uniformly, so the channel is shared
/// by bare PV + `(pipeline, Q)` regardless of how the options were
/// expressed.
fn link_pv_name(s: &str) -> &str {
    let bare = s.split(PVAJSON_IDENTITY_SEP).next().unwrap_or(s);
    let no_query = strip_query(bare);
    no_query.split_whitespace().next().unwrap_or(no_query)
}

/// Lazily register INP link options (field, sevr, proc, Q, …) from a
/// query-string-bearing name into `link_options` so `inp_cfg_for`
/// returns the right config on the first call for this link. `full`
/// is the full link string including query (e.g.
/// `"PV?field=F&proc=CPP"`). Only called when `full` contains `?`.
///
/// keyed by `full` (not bare PV name) so two links to the
/// same PV with different options each get their own entry.
/// pvxs parity: pvalink_jlif.cpp:24-196.
///
/// A structured-JSON identity key (`PV<SEP>k=kind:v…`) is NEVER
/// lazy-registered here: it is always pre-registered at link-open time
/// (`open_json_link_for_record`) under that exact key, and lenient
/// URI-parsing `pva://PV<SEP>…` would mis-read its `key=kind:value`
/// payload as a `?query`. Lazy registration handles only the
/// convenience-URI (`?query`) and legacy whitespace-suffix forms.
fn lazy_register_inp_opts(
    link_options: &parking_lot::RwLock<std::collections::HashMap<String, PvaLinkConfig>>,
    full: &str,
) {
    if full.contains(PVAJSON_IDENTITY_SEP) {
        return;
    }
    if link_options.read().contains_key(full) {
        return;
    }
    if let Ok(cfg) = PvaLinkConfig::parse(&format!("pva://{full}"), LinkDirection::Inp) {
        link_options.write().insert(
            full.to_string(),
            PvaLinkConfig {
                monitor: true,
                ..cfg
            },
        );
    }
}

/// Lazily register OUT link options from a query-string-bearing name
/// into `out_link_options`. Mirrors `lazy_register_inp_opts` for the
/// OUT direction, including the structured-JSON identity-key guard.
fn lazy_register_out_opts(
    out_link_options: &parking_lot::RwLock<std::collections::HashMap<String, PvaLinkConfig>>,
    full: &str,
) {
    if full.contains(PVAJSON_IDENTITY_SEP) {
        return;
    }
    if out_link_options.read().contains_key(full) {
        return;
    }
    if let Ok(cfg) = PvaLinkConfig::parse(&format!("pva://{full}"), LinkDirection::Out) {
        out_link_options.write().insert(full.to_string(), cfg);
    }
}

fn default_inp_cfg(pv_name: &str) -> PvaLinkConfig {
    PvaLinkConfig {
        monitor: true,
        ..PvaLinkConfig::defaults_for(pv_name, LinkDirection::Inp)
    }
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
    // value (`pvalink_lset.cpp:278-279`). The standard NTNDArray carries
    // its pixels in a discriminated union `value` member; without this an
    // INP pvalink pointed at an NTNDArray reported no usable value.
    // Idempotent on plain scalars/arrays/structures.
    match field.deref_selected() {
        PvField::Scalar(sv) => Some(scalar_to_epics(sv)),
        PvField::Structure(s) => {
            // NTEnum (pvxs `pvalink_lset.cpp:331`, gated on
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
                    // pvxs `pvaGetValue` reads the sibling `choices`
                    // string[] alongside `index` (`pvalink_lset.cpp:344-356`)
                    // so a later type-aware `put_field` can store the
                    // label on a DBR_STRING target or the index on a
                    // numeric one. The resolver boundary is dbrType-blind,
                    // so carry both through `EnumWithChoices`; an
                    // absent/empty `choices` makes the string-target path
                    // fall back to the "%u" index form.
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
            // pvxs `pvalink_lset.cpp:287` handles every pvData
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
                // `pvxs/ioc/pvalink_lset.cpp:287-325`).
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
                TypedScalarArray::Double(a) => Some(EpicsValue::DoubleArray(a.to_vec())),
                TypedScalarArray::Float(a) => Some(EpicsValue::FloatArray(a.to_vec())),
                TypedScalarArray::Int(a) => Some(EpicsValue::LongArray(a.to_vec())),
                // a remote `long[]` is 64-bit per element;
                // preserve the full width as `Int64Array`.
                TypedScalarArray::Long(a) => Some(EpicsValue::Int64Array(a.to_vec())),
                TypedScalarArray::Short(a) => Some(EpicsValue::ShortArray(a.to_vec())),
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
                TypedScalarArray::ULong(a) => Some(EpicsValue::UInt64Array(a.to_vec())),
                TypedScalarArray::Byte(a) => Some(EpicsValue::ShortArray(
                    a.iter().map(|v| *v as i16).collect(),
                )),
                TypedScalarArray::UByte(a) => Some(EpicsValue::CharArray(a.to_vec())),
                TypedScalarArray::String(a) => Some(EpicsValue::StringArray(a.to_vec())),
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
        // `TypeCode::UInt32 => DBF_ULONG`, `pvxs/ioc/pvalink_lset.cpp:215-223`).
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
        // (pvxs `pvalink_lset.cpp:330-360`, `case DBR_STRING`) while every
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
    /// `pvalink_lset.cpp:278-279`). Pre-fix a `PvField::Union` hit the
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
            Some(EpicsValue::FloatArray(vec![1.5, 2.5, 3.5])),
            "selected union must convert to its floatValue member"
        );
        // And on the whole NTNDArray struct: the `value`-child recursion
        // dereferences the union too.
        let mut nd = PvStructure::new("epics:nt/NTNDArray:1.0");
        nd.fields.push(("value".into(), union));
        assert_eq!(
            pvfield_to_epics_value(&PvField::Structure(nd)),
            Some(EpicsValue::FloatArray(vec![1.5, 2.5, 3.5])),
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
            Some(EpicsValue::FloatArray(vec![1.5, -2.5]))
        );
        assert_eq!(
            pvfield_to_epics_value(&PvField::ScalarArray(vec![
                ScalarValue::Short(-7),
                ScalarValue::Short(8),
            ])),
            Some(EpicsValue::ShortArray(vec![-7, 8]))
        );
        assert_eq!(
            pvfield_to_epics_value(&PvField::ScalarArray(vec![
                ScalarValue::UByte(0x55),
                ScalarValue::UByte(0xFF),
            ])),
            Some(EpicsValue::CharArray(vec![0x55, 0xFF]))
        );
        // pvByte → ShortArray (signed widen).
        assert_eq!(
            pvfield_to_epics_value(&PvField::ScalarArray(vec![
                ScalarValue::Byte(-1),
                ScalarValue::Byte(2),
            ])),
            Some(EpicsValue::ShortArray(vec![-1, 2]))
        );
        assert_eq!(
            pvfield_to_epics_value(&PvField::ScalarArray(vec![
                ScalarValue::String("a".into()),
                ScalarValue::String("b".into()),
            ])),
            Some(EpicsValue::StringArray(vec!["a".into(), "b".into()]))
        );

        // Typed-fast-path variants emitted by the wire decoder.
        assert_eq!(
            pvfield_to_epics_value(&PvField::ScalarArrayTyped(TypedScalarArray::Float(
                vec![3.25f32, -4.5].into()
            ))),
            Some(EpicsValue::FloatArray(vec![3.25, -4.5]))
        );
        assert_eq!(
            pvfield_to_epics_value(&PvField::ScalarArrayTyped(TypedScalarArray::String(
                vec!["x".into(), "y".into()].into()
            ))),
            Some(EpicsValue::StringArray(vec!["x".into(), "y".into()]))
        );
        assert_eq!(
            pvfield_to_epics_value(&PvField::ScalarArrayTyped(TypedScalarArray::UByte(
                vec![1u8, 2, 3].into()
            ))),
            Some(EpicsValue::CharArray(vec![1, 2, 3]))
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
            Some(EpicsValue::UInt64Array(vec![big_u, 1])),
            "remote ulong[] must keep all 64 bits per element"
        );
        // Untyped `long[]` → `Int64Array`.
        assert_eq!(
            pvfield_to_epics_value(&PvField::ScalarArray(vec![
                ScalarValue::Long(big_i),
                ScalarValue::Long(-1),
            ])),
            Some(EpicsValue::Int64Array(vec![big_i, -1])),
            "remote long[] must keep all 64 bits per element"
        );

        // Typed-fast-path `ulong[]` → `UInt64Array`.
        assert_eq!(
            pvfield_to_epics_value(&PvField::ScalarArrayTyped(TypedScalarArray::ULong(
                vec![big_u, 2].into()
            ))),
            Some(EpicsValue::UInt64Array(vec![big_u, 2])),
            "typed remote ulong[] must keep all 64 bits per element"
        );
        // Typed-fast-path `long[]` → `Int64Array`.
        assert_eq!(
            pvfield_to_epics_value(&PvField::ScalarArrayTyped(TypedScalarArray::Long(
                vec![big_i, -2].into()
            ))),
            Some(EpicsValue::Int64Array(vec![big_i, -2])),
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
    /// `pvxs/ioc/pvalink_lset.cpp:215-223`, `:287-325`).
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
            Some(EpicsValue::Int64Array(vec![big_i, 1])),
            "remote uint[] above i32::MAX must stay unsigned per element"
        );

        // Typed-fast-path `uint[]` → `Int64Array`.
        assert_eq!(
            pvfield_to_epics_value(&PvField::ScalarArrayTyped(TypedScalarArray::UInt(
                vec![big, 2].into()
            ))),
            Some(EpicsValue::Int64Array(vec![big_i, 2])),
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
            record: "DEST".to_string(),
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
    /// pvxs `ScanTrack::scan` (`pvalink_channel.cpp:316-323`,
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
            record: "DEST".to_string(),
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

    /// Attach-time scan (pvxs `pvalink_lset.cpp:148-167`): attaching a
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

        let resolver = PvaLinkResolver::new();
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
            .open_inp_cfg(cfg, "SRC".to_string(), Some("DEST".to_string()))
            .await
            .unwrap();

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

        let resolver = PvaLinkResolver::new();
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
            .open_inp_cfg(cfg, "SRC".to_string(), Some("DEST".to_string()))
            .await
            .unwrap();

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
            record: "DEST".to_string(),
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
            record: "DEST".to_string(),
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
    /// (`pvxs/ioc/pvalink_channel.cpp:359-373` + `:420-432`).
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
            record: "CP_REC".to_string(),
            monorder: 0,
            atomic: false,
            passive_only: false,
        });
        // CPP: passive-restricted; owner is Passive so it is eligible.
        fanout.records.push(ScanTarget {
            record: "CPP_REC".to_string(),
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
        {
            let rec = db.get_record("DEST").expect("DEST exists");
            let mut inst = rec.write();
            inst.put_common_field("FLNK", EpicsValue::String("DOWNSTREAM".into()))
                .expect("set FLNK");
        }

        let mut fanout = ScanFanout::default();
        fanout.records.push(ScanTarget {
            record: "DEST".to_string(),
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
        let resolver = PvaLinkResolver::new();
        // proc=CP → scan_on_update; sevr=MS exercised together.
        let _ = resolver
            .open_link_for_record("pva://SRC:PV?proc=CP&sevr=MS", "MY:REC")
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
        let resolver = PvaLinkResolver::new();
        let _ = resolver
            .open_link_for_record("pva://OTHER:PV?proc=NPP", "REC2")
            .await;
        assert!(resolver.scan_targets.read().get(&mk("OTHER:PV")).is_none());
    }

    /// B2 through the resolver: `open_link` retains `sevr` so a later
    /// full-string `inp_cfg_for` query reflects the `MSI` mode.
    /// key is the full query-bearing string, not the bare PV name.
    #[tokio::test]
    async fn b2_open_link_retains_sevr_mode() {
        let resolver = PvaLinkResolver::new();
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
        let resolver = PvaLinkResolver::new();

        // A connected INP link (for_test seeds a cached value, so
        // is_connected() reads true), a pending INP link (no value yet),
        // and an OUT link (must be excluded).
        let connected_cfg = resolver.inp_cfg_for("FR15:CONNECTED");
        let pending_cfg = resolver.inp_cfg_for("FR15:PENDING");
        let out_cfg = resolver.out_cfg_for("FR15:OUT");
        resolver.registry.insert_for_test(
            &connected_cfg,
            Arc::new(PvaLink::for_test(
                connected_cfg.clone(),
                Some(PvField::Scalar(ScalarValue::Double(1.0))),
            )),
        );
        resolver.registry.insert_for_test(
            &pending_cfg,
            Arc::new(PvaLink::for_test(pending_cfg.clone(), None)),
        );
        resolver
            .registry
            .insert_for_test(&out_cfg, Arc::new(PvaLink::for_test(out_cfg.clone(), None)));

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
            LinkSet::is_connected(&resolver, "FR15:CONNECTED"),
            "cached INP link reports connected"
        );
        assert!(
            !LinkSet::is_connected(&resolver, "FR15:PENDING"),
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
            record: "C".into(),
            monorder: 1,
            atomic: false,
            passive_only: false,
        });
        fanout.records.push(ScanTarget {
            record: "D".into(),
            monorder: -1,
            atomic: false,
            passive_only: false,
        });
        // Atomic, monorder 5 / 0.
        fanout.records.push(ScanTarget {
            record: "A".into(),
            monorder: 5,
            atomic: true,
            passive_only: false,
        });
        fanout.records.push(ScanTarget {
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
    /// (`ioc/pvalink_lset.cpp:63-75`).
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
    /// `pvaOpenLink()` for every direction (`ioc/pvalink_lset.cpp:69-74`);
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
            "pva://NOT:A:LOCAL:RECORD?local=true",
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
    /// `dbChannelTest("x.VAL{json:true}")` (`dbChannelTest.c:183-186`).
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
    /// Upstream parity: pvxs pvalink_jlif.cpp:24-196 (all pvalink JSON
    /// keys parsed as JLink map keys / typed values and stored on the
    /// jlink struct for the link's lifetime); :286-300 (no `?key=value`
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
            j,
            PvaJsonLink {
                pv: "TARGET:AI".to_string(),
                options: vec![
                    (
                        "field".to_string(),
                        JlinkValue::Str("display.precision".to_string())
                    ),
                    ("proc".to_string(), JlinkValue::Str("CPP".to_string())),
                    ("sevr".to_string(), JlinkValue::Str("MS".to_string())),
                ],
            },
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
        let resolver = PvaLinkResolver::new();
        let _ = resolver
            .open_json_link_for_record(&j.pv, &j.options, "MY:RECORD")
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
            ParsedLink::Pva(s) => s,
            other => panic!("expected Pva, got {other:?}"),
        };
        assert_eq!(
            stored, "TARGET:AI CPP MS",
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
        let resolver = PvaLinkResolver::new();
        let _ = resolver
            .open_link_for_record(&format!("pva://{stored}"), "MY:RECORD")
            .await;

        // inp_cfg_for keyed by the full suffix string returns the parsed
        // config — bare PV name, MS, CPP — not the default for a PV
        // literally named "TARGET:AI CPP MS".
        let cfg = resolver.inp_cfg_for(stored.as_str());
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
        let resolver = PvaLinkResolver::new();

        // Link A: read sub-field "alarm.severity", CPP (scan on passive).
        let link_a = "pva://TARGET:PV?field=alarm.severity&proc=CPP";
        let _ = resolver.open_link_for_record(link_a, "RECORD:A").await;

        // Link B: read sub-field "value", CP (always scan).
        let link_b = "pva://TARGET:PV?field=value&proc=CP";
        let _ = resolver.open_link_for_record(link_b, "RECORD:B").await;

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

        let resolver = PvaLinkResolver::new();

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
            .open_json_link_for_record("TARGET:PV", &opts_a, "RECORD:A")
            .await;
        let _ = resolver
            .open_json_link_for_record("TARGET:PV", &opts_b, "RECORD:B")
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
    /// `pvxs/ioc/pvalink.h:115-120`), exactly as the convenience-URI form
    /// in [`Self::br128_distinct_q_variants_do_not_collapse`]. Their CP
    /// scan fan-outs must land in SEPARATE [`MonitorKey`] buckets even
    /// though both arrive through the JSON identity-key boundary.
    #[tokio::test]
    async fn br_json_distinct_q_variants_do_not_collapse() {
        let resolver = PvaLinkResolver::new();

        let opts_q1: Vec<(String, JlinkValue)> = vec![
            ("proc".to_string(), JlinkValue::Str("CP".to_string())),
            ("Q".to_string(), JlinkValue::Int(1)),
        ];
        let opts_q64: Vec<(String, JlinkValue)> = vec![
            ("proc".to_string(), JlinkValue::Str("CP".to_string())),
            ("Q".to_string(), JlinkValue::Int(64)),
        ];
        let _ = resolver
            .open_json_link_for_record("VAR:PV", &opts_q1, "REC:Q1")
            .await;
        let _ = resolver
            .open_json_link_for_record("VAR:PV", &opts_q64, "REC:Q64")
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

        let resolver = PvaLinkResolver::new();

        let opts_a: Vec<(String, JlinkValue)> =
            vec![("field".to_string(), JlinkValue::Str("a.b".to_string()))];
        let opts_b: Vec<(String, JlinkValue)> =
            vec![("field".to_string(), JlinkValue::Str("c.d".to_string()))];
        let _ = resolver
            .open_json_out_link("OUT:PV", &opts_a, Some("WRITER:A"))
            .await;
        let _ = resolver
            .open_json_out_link("OUT:PV", &opts_b, Some("WRITER:B"))
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
    /// pvxs subscriptions, `pvxs/ioc/pvalink.h:115-120`). Their CP scan
    /// fan-out must land in SEPARATE [`MonitorKey`] buckets — pre-fix
    /// both collapsed onto one bare-PV-name entry, so a `Q=1` record
    /// could be driven by the `Q=64` monitor's events, and only one
    /// forwarder spawned for both. The base iocInit wait
    /// (`link_names`) must likewise list BOTH variants so each monitor's
    /// connection is awaited independently.
    #[tokio::test]
    async fn br128_distinct_q_variants_do_not_collapse() {
        let resolver = PvaLinkResolver::new();

        // Same PV, CP scan, but different queue depths → two variants.
        let _ = resolver
            .open_link_for_record("pva://VAR:PV?proc=CP&Q=1", "REC:Q1")
            .await;
        let _ = resolver
            .open_link_for_record("pva://VAR:PV?proc=CP&Q=64", "REC:Q64")
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
    /// `pvxs/ioc/pvalink_lset.cpp:99-122`). Each must open its OWN
    /// cached link (its own monitor receiver) and register its scan
    /// fan-out under its own [`MonitorKey`], rather than the default
    /// link satisfying both reads and CP scans for the pipelined one.
    #[tokio::test]
    async fn br37_pipeline_variant_opens_its_own_monitor() {
        let resolver = PvaLinkResolver::new();

        // Default variant (no query options) and a pipelined Q=1 variant
        // of the SAME PV, both CP so each registers a scan target.
        let _ = resolver
            .open_link_for_record("pva://PIPE:PV?proc=CP", "REC:DEF")
            .await;
        let _ = resolver
            .open_link_for_record("pva://PIPE:PV?proc=CP&Q=1&pipeline=true", "REC:PIPE")
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
    /// the atomic scan in `pvxs/ioc/pvalink_channel.cpp:422`.
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
            record: "AT:A".into(),
            monorder: 0,
            atomic: true,
            passive_only: false,
        });
        fanout.records.push(ScanTarget {
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

        let resolver = PvaLinkResolver::new();

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
            LinkSet::alarm_severity(&resolver, nms_name),
            None,
            "an NMS caller must not propagate the remote alarm"
        );
        let ms_name = "pva://MR_R15:PV?sevr=MS";
        assert_eq!(
            LinkSet::alarm_severity(&resolver, ms_name),
            Some(2),
            "an MS caller must see MAJOR even though the cached link is NMS"
        );
        assert_eq!(
            LinkSet::alarm_message(&resolver, ms_name),
            Some("HIGH".to_string()),
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
        // (`pvxs/test/testpvalink.cpp:370-428`). The earlier
        // `alarm_severity` calls read the cached value live and do NOT
        // latch, so the snapshot is still INVALID here.
        let pre = LinkSet::remote_alarm(&resolver, nms_name)
            .expect("connected link reports the initial INVALID snapshot");
        assert_eq!(pre.severity, 3, "pre-read snapshot is INVALID_ALARM");
        assert_eq!(pre.status, 14, "LINK_ALARM status");
        assert_eq!(pre.message, "", "initial snapshot carries no message");

        // A value read (the LinkSet `get_value` path = pvxs `dbGetLink` /
        // `pvaGetValue`) latches the snapshot.
        assert!(
            LinkSet::get_value(&resolver, nms_name).await.is_some(),
            "the cached value read must succeed and latch the snapshot"
        );

        // AFTER the value read: even the NMS caller — whose gated
        // contribution is None above — sees the remote MAJOR severity,
        // LINK_ALARM(14) status, and message. The `sevr` mode does not
        // gate this path.
        let snap = LinkSet::remote_alarm(&resolver, nms_name)
            .expect("NMS caller still gets the ungated remote alarm snapshot");
        assert_eq!(snap.severity, 2, "ungated snapshot reports remote MAJOR");
        assert_eq!(snap.status, 14, "LINK_ALARM status derived from severity");
        assert_eq!(snap.message, "HIGH", "ungated snapshot carries the message");

        // The shared cached link is time=false → a bare caller adopts
        // no timestamp. A caller asking `?time=true` must adopt it.
        assert_eq!(
            LinkSet::time_stamp(&resolver, nms_name),
            None,
            "a time=false caller must not adopt the upstream timestamp"
        );
        assert_eq!(
            LinkSet::time_stamp(&resolver, "pva://MR_R15:PV?time=true"),
            Some((1_700_000_000, 42, 0x0000_0000_9000_0000)),
            "a time=true caller must adopt the upstream timestamp and the \
             zero-extended userTag"
        );
    }

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
            EpicsValue::UInt64Array(vec![1, 2, u64::MAX]),
            // u64::MAX as the i64 bit pattern is -1; the test
            // compares 64-bit words, so full-width u64 is preserved.
            &[1, 2, u64::MAX as i64],
        )
        .await;
    }

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
            EpicsValue::Int64Array(vec![-3, 0, i64::MAX]),
            &[-3, 0, i64::MAX],
        )
        .await;
    }

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
        use epics_pva_rs::server_native::{PvaServer, SharedPV, SharedSource};

        // PVA server hosting a `long[]` / `ulong[]`-valued PV.
        let desc = FieldDesc::Structure {
            struct_id: "epics:nt/NTScalarArray:1.0".into(),
            fields: vec![("value".into(), FieldDesc::ScalarArray(elem_type))],
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

        let resolver = PvaLinkResolver::new();

        // Seed the registry with a pinned-client OUT link under the
        // exact key `put_value` will look up for the bare PV name, so
        // `get_or_open` is a cache hit and the typed PUT reaches the
        // pinned test server.
        let out_cfg = resolver.out_cfg_for(pv_name);
        let client = PvaClient::builder()
            .server_addr(addr)
            .timeout(std::time::Duration::from_secs(3))
            .build();
        let link = std::sync::Arc::new(PvaLink::for_test_with_client(out_cfg.clone(), client));
        resolver.registry.insert_for_test(&out_cfg, link);

        // Drive the OUT path through the resolver's LinkSet impl.
        let scheme_name = format!("pva://{pv_name}");
        LinkSet::put_value(&resolver, &scheme_name, value, LinkPutOp::Plain)
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
}
