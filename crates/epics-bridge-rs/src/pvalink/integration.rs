//! Wire pvalink up to the record-link resolver in `epics-base-rs`.
//!
//! The integration plan:
//!
//! 1. `PvaLinkResolver` owns a [`PvaLinkRegistry`] (PvaLink cache) and a
//!    [`tokio::runtime::Handle`] so the synchronous resolver closure can
//!    submit `block_on(...)` work to a real runtime.
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

use std::sync::Arc;

use epics_base_rs::server::database::{ExternalPvResolver, LinkSet, PvDatabase};
use epics_base_rs::types::EpicsValue;
use epics_pva_rs::pvdata::{PvField, ScalarValue};

use super::config::{LinkDirection, PvaLinkConfig};
use super::link::{PvaLink, PvaLinkError, PvaLinkResult};
use super::registry::PvaLinkRegistry;

/// Wrap `tokio::task::block_in_place(f)` with a runtime-flavour check.
/// Tokio's block_in_place panics under the current_thread runtime; on
/// that flavour we run `f` directly (the caller's outer block_on then
/// has nothing to fall back to and may itself fail, but we surface
/// that as a regular error rather than a panic). Used by every
/// pvalink LinkSet/Resolver entry point — they're invoked from inside
/// `PvDatabase::resolve_external_pv`'s async context.
fn block_in_place_or_warn<F, R>(f: F) -> R
where
    F: FnOnce() -> R,
{
    use tokio::runtime::{Handle, RuntimeFlavor};
    if let Ok(handle) = Handle::try_current() {
        match handle.runtime_flavor() {
            RuntimeFlavor::MultiThread => tokio::task::block_in_place(f),
            // CurrentThread (or any other future flavour) can't park
            // a worker, so we just call directly. Inside the closure
            // the caller will likely call Handle::block_on which
            // panics on current_thread; catch_unwind would mask real
            // bugs, so we let it propagate. Production IOC binaries
            // use the multi-threaded runtime.
            _ => f(),
        }
    } else {
        f()
    }
}

/// Resolver wrapping a [`PvaLinkRegistry`] and a tokio runtime handle.
/// Cheap to clone — both fields are `Arc`-backed.
#[derive(Clone)]
pub struct PvaLinkResolver {
    registry: Arc<PvaLinkRegistry>,
    handle: tokio::runtime::Handle,
    /// Counter incremented on every successful link read. Used by
    /// `pvxrefdiff` to report "links touched since last call". Wraps
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
    /// Database handle used by the B3 scan-on-update forwarder to
    /// process owning records. `None` until [`install_pvalink_resolver`]
    /// wires it — without it, monitor events still update the cached
    /// value but cannot drive `CP`/`CPP` record processing.
    db: Arc<parking_lot::RwLock<Option<PvDatabase>>>,
    /// B3: per-PV set of record names to process when a monitor event
    /// arrives (the `scan_on_update` / CP fan-out targets). Populated
    /// by [`Self::open_link_for_record`].
    scan_targets: Arc<parking_lot::RwLock<std::collections::HashMap<String, ScanFanout>>>,
    /// B3: PV names whose monitor-notification forwarder task is
    /// already running, so [`Self::open_link`] spawns it at most once
    /// per link.
    forwarders: Arc<parking_lot::Mutex<std::collections::HashSet<String>>>,
    /// B4 `local`: optional handle to the QSRV provider's name
    /// registry. When the IOC also runs QSRV (the common dual-server
    /// deployment), a `local=true` link may target a QSRV group
    /// composite PV — which lives only in the provider's group
    /// registry, not the `PvDatabase`. Without this handle the
    /// locality check sees only records / simple PVs and wrongly
    /// rejects a group-PV link with `NotLocal`. `None` for a
    /// pvalink-only deployment with no QSRV, where group-PV locality
    /// is simply unavailable. Wired via [`Self::with_qsrv_provider`].
    #[cfg(feature = "qsrv")]
    qsrv: Arc<parking_lot::RwLock<Option<Arc<crate::qsrv::BridgeProvider>>>>,
}

/// Per-PV scan-on-update fan-out state (B3).
#[derive(Default)]
struct ScanFanout {
    /// Records to process on every monitor event. Each entry mirrors
    /// one INP pvalink whose `proc` is `CP` (always) or `CPP`
    /// (passive). At our integration granularity both reduce to
    /// "process this record"; `always` widens it to fire even when
    /// the value did not change.
    records: Vec<ScanTarget>,
}

/// One record bound to a CP/CPP pvalink (B3).
struct ScanTarget {
    record: String,
    /// pvxs `pvaLinkConfig::always` — process even on a no-op update.
    always: bool,
    /// pvxs `pvaLinkConfig::monorder` — lower scans first within one
    /// monitor batch.
    monorder: i32,
    /// pvxs `pvaLinkConfig::atomic` — atomic links scan as one
    /// contiguous group ahead of the non-atomic targets in the same
    /// monitor batch, and an atomic group scans whenever *any* of its
    /// members changed (so siblings stay consistent within the batch).
    atomic: bool,
    /// BR-R28: pvxs distinguishes `CP` (scanOnUpdateYes) from `CPP`
    /// (scanOnUpdatePassive). `CPP` is gated by the owning record's
    /// SCAN being Passive (pvalink_channel.cpp:313). True here means
    /// "skip processing when the owning record's SCAN != Passive".
    passive_only: bool,
}

impl PvaLinkResolver {
    pub fn new(handle: tokio::runtime::Handle) -> Self {
        Self {
            registry: Arc::new(PvaLinkRegistry::new()),
            handle,
            reads: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            enabled: Arc::new(std::sync::atomic::AtomicBool::new(true)),
            link_options: Arc::new(parking_lot::RwLock::new(std::collections::HashMap::new())),
            db: Arc::new(parking_lot::RwLock::new(None)),
            scan_targets: Arc::new(parking_lot::RwLock::new(std::collections::HashMap::new())),
            forwarders: Arc::new(parking_lot::Mutex::new(std::collections::HashSet::new())),
            #[cfg(feature = "qsrv")]
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
    #[cfg(feature = "qsrv")]
    pub fn attach_qsrv_provider(&self, provider: Arc<crate::qsrv::BridgeProvider>) {
        *self.qsrv.write() = Some(provider);
    }

    /// Builder form of [`Self::attach_qsrv_provider`] — wires the
    /// QSRV provider and returns `self` for chaining at IOC assembly.
    #[cfg(feature = "qsrv")]
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
        // An INP link that is meaningful for the resolver must keep a
        // monitor open; force it on (pvxs treats `proc=CP/CPP` and the
        // resolver path as monitored).
        let cfg = PvaLinkConfig {
            monitor: true,
            ..cfg
        };
        let pv_name = cfg.pv_name.clone();
        // B4 `local`: a `local`-flagged link must resolve a PV served
        // by *this* IOC. pvxs requires a local channel; here that
        // means the target must be hosted by this IOC under one of
        // three forms:
        //   * a `PvDatabase` record,
        //   * a simple PV registered via `add_pv` (e.g. a QSRV
        //     single-record channel, an iocsh stats PV, a gateway
        //     shadow PV),
        //   * a QSRV group composite PV — which lives only in the
        //     QSRV provider's group registry, not the `PvDatabase`.
        // Gating on `get_record` alone wrongly rejected simple PVs;
        // gating on the `PvDatabase` alone wrongly rejected group
        // PVs. All three are IOC-local; only a genuinely remote PV is
        // rejected. The DB lookups use the no-resolver path so the
        // `local` check never triggers a remote search. Reject
        // up-front so the operator gets a clear error instead of a
        // silent remote resolution. Mirrors pvxs `pvaLinkConfig::local`.
        if cfg.local {
            // `mut` is only consumed by the QSRV fallthrough below;
            // gate it so a `qsrv`-less build does not warn unused_mut.
            #[cfg(feature = "qsrv")]
            let mut is_local = self.is_local_in_db(&pv_name).await;
            #[cfg(not(feature = "qsrv"))]
            let is_local = self.is_local_in_db(&pv_name).await;
            // QSRV group / single composite PVs: only checked when a
            // QSRV provider is wired. `hosts_pv` covers both the group
            // registry and the provider's single-channel name set.
            #[cfg(feature = "qsrv")]
            if !is_local {
                let provider = self.qsrv.read().clone();
                if let Some(provider) = provider {
                    is_local = provider.hosts_pv(&pv_name).await;
                }
            }
            if !is_local {
                return Err(PvaLinkError::NotLocal(pv_name));
            }
        }
        self.link_options
            .write()
            .insert(pv_name.clone(), cfg.clone());
        // B3: register the scan-on-update target before opening so the
        // forwarder spawned below already sees it.
        if let Some(rec) = record {
            if cfg.scan_on_update {
                self.scan_targets
                    .write()
                    .entry(pv_name.clone())
                    .or_default()
                    .records
                    .push(ScanTarget {
                        record: rec,
                        always: cfg.always,
                        monorder: cfg.monorder,
                        atomic: cfg.atomic,
                        passive_only: cfg.scan_on_passive,
                    });
            }
        }
        let link = self.registry.get_or_open(cfg).await?;
        self.spawn_notify_forwarder(&pv_name, &link);
        Ok(link)
    }

    /// B3: spawn the per-link monitor-notification forwarder, at most
    /// once per PV. The task drains the link's notify receiver and,
    /// for every event, processes the link's registered
    /// scan-on-update records (`monorder`-sorted; `always` links also
    /// process on no-op updates).
    fn spawn_notify_forwarder(&self, pv_name: &str, link: &Arc<PvaLink>) {
        {
            let mut started = self.forwarders.lock();
            if started.contains(pv_name) {
                return;
            }
            started.insert(pv_name.to_string());
        }
        let Some(rx) = link.take_notify_rx() else {
            // OUT / non-monitor links never created a channel.
            self.forwarders.lock().remove(pv_name);
            return;
        };
        let pv_name = pv_name.to_string();
        let scan_targets = self.scan_targets.clone();
        let db = self.db.clone();
        let field = link.config().field.clone();
        self.handle
            .spawn(run_notify_forwarder(pv_name, field, rx, scan_targets, db));
    }

    /// Whether `pv_name` is hosted by the attached `PvDatabase` as a
    /// record or a simple `add_pv` PV. Both lookups use the
    /// no-resolver path so the `local` check never triggers a remote
    /// search. Returns `false` when no database is attached.
    async fn is_local_in_db(&self, pv_name: &str) -> bool {
        // Clone the Option out and drop the RwLock guard before any
        // await — holding a parking_lot guard across an await point
        // can stall or deadlock the executor.
        let db = self.db.read().clone();
        match db {
            Some(db) => {
                db.get_record_no_resolve(pv_name).await.is_some()
                    || db.find_pv(pv_name).await.is_some()
            }
            None => false,
        }
    }

    /// Build the INP config for `pv_name`, applying any options
    /// registered via [`Self::open_link`]. Falls back to the pvxs
    /// monitor defaults (`NMS`, `Q=4`, no pipeline) when none.
    fn inp_cfg_for(&self, pv_name: &str) -> PvaLinkConfig {
        if let Some(cfg) = self.link_options.read().get(pv_name) {
            return PvaLinkConfig {
                monitor: true,
                ..cfg.clone()
            };
        }
        default_inp_cfg(pv_name)
    }

    /// Number of successful link reads since startup.
    pub fn read_count(&self) -> u64 {
        self.reads.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Number of cached links.
    pub fn link_count(&self) -> usize {
        self.registry.len()
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
        let name = strip_scheme(pv_name)?;
        self.registry
            .try_get(name, LinkDirection::Inp)?
            .link_alarm_severity()
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
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
    }

    /// Build the [`ExternalPvResolver`] closure that the database
    /// expects. The closure is sync; it uses
    /// `Handle::block_on(future)` for the rare uncached path.
    /// Pre-warm INP links via [`Self::open`] to keep the steady-state
    /// path lock-free. The returned closure has a sync fast path
    /// (cache hit on a pre-warmed monitor) and only falls through
    /// to `block_on` on the first call for a given PV.
    pub fn build_resolver(self) -> ExternalPvResolver {
        let resolver = self;
        Arc::new(move |name: &str| -> Option<EpicsValue> {
            if !resolver.is_enabled() {
                return None;
            }
            // Strip optional pva:// prefix — the resolver receives the
            // bare PV name in some link forms but the prefixed form in
            // others. `ca://` is handled by libca, not pvalink — reject.
            let name = match name.strip_prefix("pva://") {
                Some(stripped) => stripped,
                None => {
                    if name.starts_with("ca://") {
                        return None;
                    }
                    name
                }
            };

            // Fast path: a previously-opened link with a cached
            // monitor value. No `block_on`, no async runtime touch.
            if let Some(link) = resolver.registry.try_get(name, LinkDirection::Inp)
                && let Some(value) = link.try_read_cached()
            {
                resolver
                    .reads
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                return pvfield_to_epics_value(&value);
            }

            // Slow path: link not yet open or first-event not arrived.
            // Open the link (idempotent) then issue an async read.
            // B4: use `inp_cfg_for` so a link registered via
            // `open_link` keeps its options (`sevr`, `Q`, `pipeline`,
            // `monorder`); `default_inp_cfg` would discard them.
            let cfg = resolver.inp_cfg_for(name);
            // The Lset external resolver is invoked from inside an
            // async context (PvDatabase::resolve_external_pv runs on a
            // tokio worker). Bare Handle::block_on panics under those
            // conditions. block_in_place yields the worker thread for
            // the duration of the inner block_on so the runtime stays
            // healthy. Requires the multi-threaded runtime, which is
            // the only flavour our IOC binaries use.
            let (link, value) = block_in_place_or_warn(|| {
                resolver.handle.block_on(async {
                    let link = resolver.registry.get_or_open(cfg).await.ok()?;
                    let value = link.read().await.ok()?;
                    Some((link, value))
                })
            })?;
            let _ = link;
            resolver
                .reads
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            pvfield_to_epics_value(&value)
        })
    }
}

/// Install a [`PvaLinkResolver`] on `db`. Returns the resolver so the
/// caller can pre-open links and query stats (`db_pvxr` / `pvxrefdiff`
/// iocsh commands lean on this).
///
/// Registers the resolver under the `"pva"` lset scheme *and*
/// installs the legacy [`ExternalPvResolver`] closure so callers
/// using either dispatch path work transparently.
pub async fn install_pvalink_resolver(
    db: &Arc<PvDatabase>,
    handle: tokio::runtime::Handle,
) -> PvaLinkResolver {
    let resolver = PvaLinkResolver::new(handle);
    // B3: give the resolver the DB handle so the scan-on-update
    // forwarder can process owning records.
    resolver.attach_database((**db).clone());
    db.set_external_resolver(resolver.clone().build_resolver())
        .await;
    db.register_link_set("pva", Arc::new(resolver.clone()))
        .await;
    resolver
}

type ScanTargetMap = Arc<parking_lot::RwLock<std::collections::HashMap<String, ScanFanout>>>;

/// B3 monitor-notification forwarder loop.
///
/// Drains `rx` (fed by the link's monitor task) and, for every event,
/// processes the records registered as scan-on-update targets for
/// `pv_name`.
///
/// Ordering (B4): `atomic` targets scan first as one contiguous
/// group, then the non-atomic targets; within each group `monorder`
/// (low → high) decides the order. A `CPP` target (`always=false`) is
/// skipped on a no-op update, *except* an atomic target scans
/// whenever any atomic sibling in the same batch changed — so atomic
/// links stay mutually consistent. Mirrors pvxs `pvaLinkConfig`
/// `atomic` / `monorder` / `always`. The loop ends when every sender
/// is dropped (i.e. the link is closed).
async fn run_notify_forwarder(
    pv_name: String,
    field: String,
    mut rx: tokio::sync::mpsc::Receiver<PvField>,
    scan_targets: ScanTargetMap,
    db: Arc<parking_lot::RwLock<Option<PvDatabase>>>,
) {
    // Last delivered leaf value, so `always=false` targets can be
    // skipped on a no-op update (pvxs `pvaLinkConfig::always`).
    let mut last: Option<PvField> = None;
    while let Some(value) = rx.recv().await {
        let leaf = extract_leaf(&value, &field);
        let changed = last.as_ref() != Some(&leaf);
        last = Some(leaf);

        // Snapshot the fan-out, then order it: atomic group first,
        // then non-atomic; `monorder` within each group.
        let mut targets: Vec<(String, bool, i32, bool, bool)> =
            match scan_targets.read().get(&pv_name) {
                Some(fanout) => fanout
                    .records
                    .iter()
                    .map(|t| {
                        (
                            t.record.clone(),
                            t.always,
                            t.monorder,
                            t.atomic,
                            t.passive_only,
                        )
                    })
                    .collect(),
                None => Vec::new(),
            };
        // Sort key: (!atomic, monorder) → atomic (false sorts first),
        // then ascending monorder.
        targets.sort_by_key(|(_, _, order, atomic, _)| (!*atomic, *order));

        let Some(db_handle) = db.read().clone() else {
            continue;
        };
        for (record, always, _order, atomic, passive_only) in targets {
            // pvxs: a CPP (`always=false`) link only scans when the
            // input value actually changed; CP with `always` scans
            // unconditionally. An atomic link scans whenever the
            // batch changed so atomic siblings stay consistent.
            if !changed && !always && !atomic {
                continue;
            }
            // BR-R28: `CPP` (passive_only) only fires when the owning
            // record's SCAN is Passive. pvxs `pvalink_channel.cpp:313`
            // checks `prec->scan != 0` and skips processing; non-zero
            // SCAN (Event, IO Intr, periodic) means the record has
            // its own scan source and must not be re-fired from CPP.
            if passive_only {
                let is_passive = match db_handle.get_record(&record).await {
                    Some(rec) => matches!(
                        rec.read().await.common.scan,
                        epics_base_rs::server::record::ScanType::Passive
                    ),
                    None => continue,
                };
                if !is_passive {
                    continue;
                }
            }
            // B3: process WITH links so the CP/CPP-driven scan fans
            // out via INP/OUT/FLNK — a pvalink feeding a calc record
            // must propagate to the calc's FLNK chain. Bare
            // `process_record` runs only `process_local` and would
            // drop the chain. Fresh `visited` set + depth 0: this is
            // the foreign-caller entry, like the scan loop and FLNK
            // dispatch.
            let mut visited = std::collections::HashSet::new();
            let _ = db_handle
                .process_record_with_links(&record, &mut visited, 0)
                .await;
        }
    }
}

/// Walk a dotted field path and return the leaf [`PvField`]. Mirror
/// of `link::extract_field` for the B3 forwarder's change detection.
fn extract_leaf(root: &PvField, path: &str) -> PvField {
    if path.is_empty() {
        return root.clone();
    }
    let mut cursor = root.clone();
    for segment in path.split('.') {
        cursor = match cursor {
            PvField::Structure(s) => s.get_field(segment).cloned().unwrap_or(PvField::Null),
            other => return other,
        };
    }
    cursor
}

impl LinkSet for PvaLinkResolver {
    fn is_connected(&self, name: &str) -> bool {
        // Sync-only check: trait can't await. If the link hasn't
        // been pre-opened we report "not connected" — the resolver
        // hot path or `pvxr` will open it lazily; any caller that
        // wants a fresh open should call `Self::open(name).await`
        // first.
        let Some(name) = strip_scheme(name) else {
            return false;
        };
        match self.registry.try_get(name, LinkDirection::Inp) {
            Some(link) => link.is_connected(),
            None => false,
        }
    }

    fn get_value(&self, name: &str) -> Option<EpicsValue> {
        if !self.is_enabled() {
            return None;
        }
        let name = strip_scheme(name)?;

        // Fast path: cached monitor value, no async runtime touch.
        if let Some(link) = self.registry.try_get(name, LinkDirection::Inp)
            && let Some(value) = link.try_read_cached()
        {
            self.reads
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            return pvfield_to_epics_value(&value);
        }

        // Slow path: open the link / fall back to a fresh GET.
        // B4: use `inp_cfg_for` so a link registered via `open_link`
        // keeps its options (`sevr`, `Q`, `pipeline`, `monorder`);
        // `default_inp_cfg` would discard them.
        let cfg = self.inp_cfg_for(name);
        let value = block_in_place_or_warn(|| {
            self.handle.block_on(async {
                let link = self.registry.get_or_open(cfg).await.ok()?;
                link.read().await.ok()
            })
        })?;
        self.reads
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        pvfield_to_epics_value(&value)
    }

    fn put_value(&self, name: &str, value: EpicsValue) -> Result<(), String> {
        if !self.is_enabled() {
            return Err("pvalink disabled".into());
        }
        let name = strip_scheme(name).ok_or_else(|| {
            format!("pvalink rejects ca:// scheme: {name} (use the CA-link path instead)")
        })?;
        let cfg = PvaLinkConfig {
            process: true,
            ..PvaLinkConfig::defaults_for(name, LinkDirection::Out)
        };
        // P-G16: bypass the Display→string→parse round-trip for
        // ARRAYS (where Display alloc is O(N_elements * digits) and
        // pvput re-parses 25 MB strings on a 1 M-element waveform).
        // SCALARS keep the string path because the typed
        // PvField::Scalar doesn't carry the upstream NT-structure
        // wrapper; encode_pv_field on a (Structure intro, Scalar
        // value) mismatch hits the fall-through arm and emits zero
        // bytes — a Round-6 regression caught immediately on
        // verification of the original P-G16 fix.
        let array_path = matches!(
            value,
            EpicsValue::ShortArray(_)
                | EpicsValue::FloatArray(_)
                | EpicsValue::EnumArray(_)
                | EpicsValue::DoubleArray(_)
                | EpicsValue::LongArray(_)
                | EpicsValue::CharArray(_)
                | EpicsValue::StringArray(_)
        );
        block_in_place_or_warn(|| {
            self.handle.block_on(async {
                let link = self
                    .registry
                    .get_or_open(cfg)
                    .await
                    .map_err(|e| e.to_string())?;
                if array_path {
                    let pv_field = crate::convert::epics_to_pv_field(&value);
                    link.write_pv_field(&pv_field)
                        .await
                        .map_err(|e| e.to_string())
                } else {
                    let value_str = value.to_string();
                    link.write(&value_str).await.map_err(|e| e.to_string())
                }
            })
        })
    }

    fn alarm_message(&self, name: &str) -> Option<String> {
        let name = strip_scheme(name)?;
        let link = block_in_place_or_warn(|| {
            self.handle
                .block_on(async { self.registry.get_or_open(default_inp_cfg(name)).await.ok() })
        })?;
        link.alarm_message()
    }

    fn alarm_severity(&self, name: &str) -> Option<i32> {
        // B2: surface the gated link-alarm severity so the owning
        // record's `LINK_ALARM` actually reflects the remote PV's
        // alarm. `PvaLink::link_alarm_severity` already applies the
        // link's `MS`/`NMS`/`MSI` maximize-severity mode, so the
        // value returned here is final — `None` means "do not
        // propagate" (NMS, or remote severity below the mode's
        // threshold, or no value cached).
        let name = strip_scheme(name)?;
        let link = block_in_place_or_warn(|| {
            self.handle
                .block_on(async { self.registry.get_or_open(default_inp_cfg(name)).await.ok() })
        })?;
        link.link_alarm_severity()
    }

    fn time_stamp(&self, name: &str) -> Option<(i64, i32)> {
        let name = strip_scheme(name)?;
        let link = block_in_place_or_warn(|| {
            self.handle.block_on(async {
                // BR-R19: prefer the operator-installed link config so
                // `?time=true` gates the lookup; fall back to defaults
                // for bare auto-resolved links (which never adopt
                // upstream time — matching pvxs `pvaLinkConfig::time`
                // default false).
                let cfg = self.inp_cfg_for(name);
                self.registry.get_or_open(cfg).await.ok()
            })
        })?;
        // BR-R19: only adopt the upstream timestamp when the link was
        // configured with `time=true`. pvxs `pvalink_lset.cpp:427`
        // copies the latched remote NT timestamp into the owning
        // record only when this flag is set; otherwise the owning
        // record keeps its locally-stamped processing time.
        if !link.config().time {
            return None;
        }
        link.time_stamp()
    }

    fn link_names(&self) -> Vec<String> {
        // The registry is keyed on (pv_name, direction). We don't
        // currently expose iteration; skip for now and rely on
        // resolver-level stats (read_count / link_count) for
        // dbpvxr summaries.
        Vec::new()
    }
}

/// Strip the `pva://` scheme prefix the bridge sometimes prepends.
/// Pvalink only handles PVA — `ca://` is the libca scheme and is
/// dispatched by the CA-link path elsewhere, so an explicit `ca://`
/// here returns `None` so the caller can short-circuit. Names with
/// no scheme are passed through.
fn strip_scheme(name: &str) -> Option<&str> {
    if let Some(stripped) = name.strip_prefix("pva://") {
        return Some(stripped);
    }
    if name.starts_with("ca://") {
        return None;
    }
    Some(name)
}

fn default_inp_cfg(pv_name: &str) -> PvaLinkConfig {
    PvaLinkConfig {
        monitor: true,
        ..PvaLinkConfig::defaults_for(pv_name, LinkDirection::Inp)
    }
}

/// Best-effort conversion. We coerce scalar values and 1-D scalar arrays;
/// structures collapse to their `value` field. Returns `None` for
/// unsupported shapes — callers fall back to `None` in the resolver
/// closure, which surfaces as "no link value" upstream (record alarm
/// LINK/INVALID).
fn pvfield_to_epics_value(field: &PvField) -> Option<EpicsValue> {
    match field {
        PvField::Scalar(sv) => Some(scalar_to_epics(sv)),
        PvField::Structure(s) => {
            for (name, sub) in &s.fields {
                if name == "value" {
                    return pvfield_to_epics_value(sub);
                }
            }
            None
        }
        PvField::ScalarArray(arr) => {
            // Pick the first variant — pvData ScalarArray is typed
            // homogeneous on the wire, but our PvField::ScalarArray is
            // a Vec<ScalarValue> so we walk to determine.
            let first = arr.first()?;
            match first {
                ScalarValue::Double(_) => {
                    let v: Vec<f64> = arr
                        .iter()
                        .filter_map(|s| {
                            if let ScalarValue::Double(d) = s {
                                Some(*d)
                            } else {
                                None
                            }
                        })
                        .collect();
                    Some(EpicsValue::DoubleArray(v))
                }
                ScalarValue::Int(_) => {
                    let v: Vec<i32> = arr
                        .iter()
                        .filter_map(|s| {
                            if let ScalarValue::Int(i) = s {
                                Some(*i)
                            } else {
                                None
                            }
                        })
                        .collect();
                    Some(EpicsValue::LongArray(v))
                }
                _ => None,
            }
        }
        _ => None,
    }
}

fn scalar_to_epics(sv: &ScalarValue) -> EpicsValue {
    match sv {
        ScalarValue::Double(v) => EpicsValue::Double(*v),
        ScalarValue::Float(v) => EpicsValue::Float(*v),
        ScalarValue::Long(v) => EpicsValue::Long(*v as i32),
        ScalarValue::Int(v) => EpicsValue::Long(*v),
        ScalarValue::Short(v) => EpicsValue::Short(*v),
        ScalarValue::Byte(v) => EpicsValue::Char(*v as u8),
        ScalarValue::ULong(v) => EpicsValue::Long(*v as i32),
        ScalarValue::UInt(v) => EpicsValue::Long(*v as i32),
        ScalarValue::UShort(v) => EpicsValue::Short(*v as i16),
        // F9: DBF_CHAR is signed (pvByte). Widen UByte to Short so the
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
        assert_eq!(pvfield_to_epics_value(&f), Some(EpicsValue::Long(42)));
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
        fn field_list(&self) -> &'static [epics_base_rs::server::record::FieldDesc] {
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

        // CP-style target: always=true so every event scans.
        let mut fanout = ScanFanout::default();
        fanout.records.push(ScanTarget {
            record: "DEST".to_string(),
            always: true,
            monorder: 0,
            atomic: false,
            passive_only: false,
        });
        let scan_targets: ScanTargetMap =
            Arc::new(parking_lot::RwLock::new(std::collections::HashMap::from([
                ("SRC".to_string(), fanout),
            ])));
        let db_slot = Arc::new(parking_lot::RwLock::new(Some(db)));

        let (tx, rx) = tokio::sync::mpsc::channel::<PvField>(8);
        let forwarder = tokio::spawn(run_notify_forwarder(
            "SRC".to_string(),
            "value".to_string(),
            rx,
            scan_targets,
            db_slot,
        ));

        // Two distinct values → two scans.
        tx.send(nt_scalar(1.0)).await.unwrap();
        tx.send(nt_scalar(2.0)).await.unwrap();
        drop(tx); // close channel so the forwarder loop ends
        forwarder.await.unwrap();

        assert_eq!(count.load(std::sync::atomic::Ordering::SeqCst), 2);
    }

    /// B3: with `always=false` (a `CPP` link) a no-op update — same
    /// leaf value — does NOT re-process the record.
    #[tokio::test]
    async fn b3_forwarder_skips_unchanged_value_when_not_always() {
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
            always: false,
            monorder: 0,
            atomic: false,
            passive_only: false,
        });
        let scan_targets: ScanTargetMap =
            Arc::new(parking_lot::RwLock::new(std::collections::HashMap::from([
                ("SRC".to_string(), fanout),
            ])));
        let db_slot = Arc::new(parking_lot::RwLock::new(Some(db)));

        let (tx, rx) = tokio::sync::mpsc::channel::<PvField>(8);
        let forwarder = tokio::spawn(run_notify_forwarder(
            "SRC".to_string(),
            "value".to_string(),
            rx,
            scan_targets,
            db_slot,
        ));

        // 1.0 (change), 1.0 (no-op → skipped), 3.0 (change).
        tx.send(nt_scalar(1.0)).await.unwrap();
        tx.send(nt_scalar(1.0)).await.unwrap();
        tx.send(nt_scalar(3.0)).await.unwrap();
        drop(tx);
        forwarder.await.unwrap();

        // Only the two changed events scanned.
        assert_eq!(count.load(std::sync::atomic::Ordering::SeqCst), 2);
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
            let rec = db.get_record("DEST").await.expect("DEST exists");
            let mut inst = rec.write().await;
            inst.put_common_field("FLNK", EpicsValue::String("DOWNSTREAM".into()))
                .expect("set FLNK");
        }

        let mut fanout = ScanFanout::default();
        fanout.records.push(ScanTarget {
            record: "DEST".to_string(),
            always: true,
            monorder: 0,
            atomic: false,
            passive_only: false,
        });
        let scan_targets: ScanTargetMap =
            Arc::new(parking_lot::RwLock::new(std::collections::HashMap::from([
                ("SRC".to_string(), fanout),
            ])));
        let db_slot = Arc::new(parking_lot::RwLock::new(Some(db)));

        let (tx, rx) = tokio::sync::mpsc::channel::<PvField>(8);
        let forwarder = tokio::spawn(run_notify_forwarder(
            "SRC".to_string(),
            "value".to_string(),
            rx,
            scan_targets,
            db_slot,
        ));

        tx.send(nt_scalar(5.0)).await.unwrap();
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
        let resolver = PvaLinkResolver::new(tokio::runtime::Handle::current());
        // proc=CP → scan_on_update; sevr=MS exercised together.
        let _ = resolver
            .open_link_for_record("pva://SRC:PV?proc=CP&sevr=MS", "MY:REC")
            .await;
        // Scan target registered under the bare PV name.
        let targets = resolver.scan_targets.read();
        let fanout = targets.get("SRC:PV").expect("scan target registered");
        assert_eq!(fanout.records.len(), 1);
        assert_eq!(fanout.records[0].record, "MY:REC");
        drop(targets);
        // Parsed options retained for the resolver hot path.
        let opts = resolver.link_options.read();
        let cfg = opts.get("SRC:PV").expect("link options retained");
        assert_eq!(cfg.sevr, SevrMode::Ms);
        assert!(cfg.scan_on_update);
    }

    /// B3: a non-CP link (`proc=NPP`) opened with a record does NOT
    /// register a scan target — only CP/CPP fan out.
    #[tokio::test]
    async fn b3_non_cp_link_registers_no_scan_target() {
        let resolver = PvaLinkResolver::new(tokio::runtime::Handle::current());
        let _ = resolver
            .open_link_for_record("pva://OTHER:PV?proc=NPP", "REC2")
            .await;
        assert!(resolver.scan_targets.read().get("OTHER:PV").is_none());
    }

    /// B2 through the resolver: `open_link` retains `sevr` so a later
    /// bare-name `link_alarm_severity` query reflects the `MS` mode.
    #[tokio::test]
    async fn b2_open_link_retains_sevr_mode() {
        let resolver = PvaLinkResolver::new(tokio::runtime::Handle::current());
        let _ = resolver.open_link("pva://A:PV?sevr=MSI").await;
        let cfg = resolver.inp_cfg_for("A:PV").clone();
        assert_eq!(cfg.sevr, SevrMode::Msi);
        // A PV never opened falls back to NMS default.
        assert_eq!(resolver.inp_cfg_for("UNSEEN").sevr, SevrMode::Nms);
    }

    #[test]
    fn extract_leaf_walks_dotted_path() {
        let mut alarm = PvStructure::new("alarm_t");
        alarm
            .fields
            .push(("severity".into(), PvField::Scalar(ScalarValue::Int(2))));
        let mut root = PvStructure::new("epics:nt/NTScalar:1.0");
        root.fields
            .push(("alarm".into(), PvField::Structure(alarm)));
        let leaf = extract_leaf(&PvField::Structure(root), "alarm.severity");
        assert!(matches!(leaf, PvField::Scalar(ScalarValue::Int(2))));
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
        fn field_list(&self) -> &'static [epics_base_rs::server::record::FieldDesc] {
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
            always: true,
            monorder: 1,
            atomic: false,
            passive_only: false,
        });
        fanout.records.push(ScanTarget {
            record: "D".into(),
            always: true,
            monorder: -1,
            atomic: false,
            passive_only: false,
        });
        // Atomic, monorder 5 / 0.
        fanout.records.push(ScanTarget {
            record: "A".into(),
            always: true,
            monorder: 5,
            atomic: true,
            passive_only: false,
        });
        fanout.records.push(ScanTarget {
            record: "B".into(),
            always: true,
            monorder: 0,
            atomic: true,
            passive_only: false,
        });
        let scan_targets: ScanTargetMap =
            Arc::new(parking_lot::RwLock::new(std::collections::HashMap::from([
                ("SRC".to_string(), fanout),
            ])));
        let db_slot = Arc::new(parking_lot::RwLock::new(Some(db)));

        let (tx, rx) = tokio::sync::mpsc::channel::<PvField>(4);
        let forwarder = tokio::spawn(run_notify_forwarder(
            "SRC".to_string(),
            "value".to_string(),
            rx,
            scan_targets,
            db_slot,
        ));
        tx.send(nt_scalar(1.0)).await.unwrap();
        drop(tx);
        forwarder.await.unwrap();

        // atomic group first (B by monorder 0, then A by 5), then
        // non-atomic (D by -1, then C by 1).
        assert_eq!(*log.lock(), vec!["B", "A", "D", "C"]);
    }

    /// B4 `atomic`: an atomic CPP-style target (`always=false`) still
    /// scans on a no-op update — atomic siblings stay consistent.
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
            always: false, // CPP
            monorder: 0,
            atomic: true, // but atomic → scans anyway
            passive_only: false,
        });
        let scan_targets: ScanTargetMap =
            Arc::new(parking_lot::RwLock::new(std::collections::HashMap::from([
                ("SRC".to_string(), fanout),
            ])));
        let db_slot = Arc::new(parking_lot::RwLock::new(Some(db)));
        let (tx, rx) = tokio::sync::mpsc::channel::<PvField>(4);
        let forwarder = tokio::spawn(run_notify_forwarder(
            "SRC".to_string(),
            "value".to_string(),
            rx,
            scan_targets,
            db_slot,
        ));
        // Two identical values: the 2nd is a no-op. A plain CPP link
        // would scan once; atomic makes it scan both times.
        tx.send(nt_scalar(1.0)).await.unwrap();
        tx.send(nt_scalar(1.0)).await.unwrap();
        drop(tx);
        forwarder.await.unwrap();
        assert_eq!(count.load(std::sync::atomic::Ordering::SeqCst), 2);
    }

    /// B4 `local`: a `local`-flagged link whose PV is not a local
    /// record is rejected at open time.
    #[tokio::test]
    async fn b4_local_link_rejects_non_local_pv() {
        let db = Arc::new(PvDatabase::new());
        let resolver = install_pvalink_resolver(&db, tokio::runtime::Handle::current()).await;
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
        let resolver = install_pvalink_resolver(&db, tokio::runtime::Handle::current()).await;
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
        let resolver = install_pvalink_resolver(&db, tokio::runtime::Handle::current()).await;
        let r = resolver.open_link("pva://LOCAL:SIMPLE:PV?local=true").await;
        assert!(
            r.is_ok(),
            "local link to a simple add_pv PV must be accepted"
        );
    }

    /// #2: a `local=true` pvalink to a QSRV group composite PV must
    /// be accepted. Group PVs live only in the QSRV provider's group
    /// registry, never the `PvDatabase`, so the record / simple-PV
    /// locality check sees nothing and would wrongly return
    /// `NotLocal`. With the QSRV provider wired via
    /// `attach_qsrv_provider`, the gate also accepts any name the
    /// provider hosts. The control case — a `local=true` link to a
    /// genuinely remote-only PV — must still be rejected.
    #[cfg(feature = "qsrv")]
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
        provider.process_groups();
        assert!(
            provider.has_group_pv("LOCAL:GROUP"),
            "group PV must be registered in the provider"
        );

        let resolver = install_pvalink_resolver(&db, tokio::runtime::Handle::current()).await;
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

    /// #2: with no QSRV provider wired (pvalink-only deployment), the
    /// `local` gate keeps its record / simple-PV behaviour — group-PV
    /// locality is simply unavailable, and a link to a non-local PV
    /// is still rejected. Guards the optionality of the QSRV handle.
    #[cfg(feature = "qsrv")]
    #[tokio::test]
    async fn b4_local_gate_without_qsrv_still_rejects_remote() {
        let db = Arc::new(PvDatabase::new());
        let resolver = install_pvalink_resolver(&db, tokio::runtime::Handle::current()).await;
        // No attach_qsrv_provider call.
        let r = resolver
            .open_link("pva://NO:QSRV:REMOTE:PV?local=true")
            .await;
        assert!(
            matches!(r, Err(PvaLinkError::NotLocal(_))),
            "without a QSRV handle a non-local link must still be rejected"
        );
    }
}
