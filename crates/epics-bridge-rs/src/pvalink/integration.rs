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
use super::link::{PvaLink, PvaLinkResult};
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
    /// the bridge resolver is consulted. To keep B2 (`MS`/`NMS`)
    /// effective on the resolver hot path, the bridge stashes the
    /// parsed [`PvaLinkConfig`] here keyed by PV name when a full
    /// link string is opened via [`Self::open_link`]; the resolver
    /// then reuses those options instead of the `NMS` default.
    /// Mirrors the role of pvxs `pvaLinkConfig` carried on the
    /// `jlink` for the lifetime of the link.
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
}

/// Per-PV scan-on-update fan-out state (B3).
#[derive(Default)]
struct ScanFanout {
    /// Records to process on every monitor event. Each entry mirrors
    /// one INP pvalink whose `proc` is `CP` (always) or `CPP`
    /// (passive). At our integration granularity both reduce to
    /// "process this record".
    records: Vec<ScanTarget>,
}

/// One record bound to a CP/CPP pvalink (B3).
struct ScanTarget {
    record: String,
    /// pvxs `pvaLinkConfig` CP vs CPP: a `CP` link processes on every
    /// event; a `CPP` link only processes when the value changed. We
    /// reduce that to "process even on a no-op update" — `CP` links
    /// set this `true`, `CPP` links `false`.
    always: bool,
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
        }
    }

    /// Attach the database handle the B3 scan-on-update forwarder
    /// uses to process owning records. Called by
    /// [`install_pvalink_resolver`].
    pub fn attach_database(&self, db: PvDatabase) {
        *self.db.write() = Some(db);
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
    /// Honors any link options previously registered for `pv_name`
    /// via [`Self::open_link`] — otherwise pvxs defaults apply.
    pub async fn open(&self, pv_name: &str) -> PvaLinkResult<Arc<PvaLink>> {
        self.registry.get_or_open(self.inp_cfg_for(pv_name)).await
    }

    /// Open / cache a link from a full `@pva://...` link string,
    /// parsing and retaining its options (`sevr`, ...). The parsed
    /// [`PvaLinkConfig`] is stashed under the bare PV name so the
    /// steady-state resolver hot path — driven by `epics-base-rs`,
    /// which only ever hands the bridge a bare PV name — keeps
    /// applying the same options.
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
                        always: true,
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
    /// scan-on-update records.
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

    /// Build the INP config for `pv_name`, applying any options
    /// registered via [`Self::open_link`]. Falls back to the pvxs
    /// monitor defaults (`NMS`) when none.
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
/// `pv_name`. A target with `always=false` (a `CPP` link) is skipped
/// when the linked leaf field did not change. The loop ends when
/// every sender is dropped (i.e. the link is closed).
async fn run_notify_forwarder(
    pv_name: String,
    field: String,
    mut rx: tokio::sync::mpsc::Receiver<PvField>,
    scan_targets: ScanTargetMap,
    db: Arc<parking_lot::RwLock<Option<PvDatabase>>>,
) {
    // Last delivered leaf value, so `always=false` targets can be
    // skipped on a no-op update.
    let mut last: Option<PvField> = None;
    while let Some(value) = rx.recv().await {
        let leaf = extract_leaf(&value, &field);
        let changed = last.as_ref() != Some(&leaf);
        last = Some(leaf);

        let targets: Vec<(String, bool)> = match scan_targets.read().get(&pv_name) {
            Some(fanout) => fanout
                .records
                .iter()
                .map(|t| (t.record.clone(), t.always))
                .collect(),
            None => Vec::new(),
        };

        let Some(db_handle) = db.read().clone() else {
            continue;
        };
        for (record, always) in targets {
            // A CPP (`always=false`) link only scans when the input
            // value actually changed; CP scans unconditionally.
            if !changed && !always {
                continue;
            }
            let _ = db_handle.process_record(&record).await;
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
                    let pv_field = crate::qsrv::convert::epics_to_pv_field(&value);
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

    fn time_stamp(&self, name: &str) -> Option<(i64, i32)> {
        let name = strip_scheme(name)?;
        let link = block_in_place_or_warn(|| {
            self.handle
                .block_on(async { self.registry.get_or_open(default_inp_cfg(name)).await.ok() })
        })?;
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
            self.count
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
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
        });
        let scan_targets: ScanTargetMap = Arc::new(parking_lot::RwLock::new(
            std::collections::HashMap::from([("SRC".to_string(), fanout)]),
        ));
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
        });
        let scan_targets: ScanTargetMap = Arc::new(parking_lot::RwLock::new(
            std::collections::HashMap::from([("SRC".to_string(), fanout)]),
        ));
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

    /// B3: `open_link_for_record` registers a `proc=CP` link's scan
    /// target and retains its parsed options (B2 `sevr` included).
    #[tokio::test]
    async fn b3_open_link_for_record_registers_scan_target() {
        let resolver = PvaLinkResolver::new(tokio::runtime::Handle::current());
        let _ = resolver
            .open_link_for_record("pva://SRC:PV?proc=CP&sevr=MS", "MY:REC")
            .await;
        let targets = resolver.scan_targets.read();
        let fanout = targets.get("SRC:PV").expect("scan target registered");
        assert_eq!(fanout.records.len(), 1);
        assert_eq!(fanout.records[0].record, "MY:REC");
        drop(targets);
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
}
