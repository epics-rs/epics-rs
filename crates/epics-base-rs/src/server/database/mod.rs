pub mod db_access;
mod field_io;
pub mod filters;
mod link_set;
mod links;
mod processing;
mod record_lock;
mod scan_index;

pub use link_set::{
    DynLinkSet, LinkDbfType, LinkMetadata, LinkPutOp, LinkSet, LinkSetRegistry, RemoteAlarm,
};
pub use processing::{AsyncDbHandle, AsyncToken};
pub use record_lock::{ManyRecordWriteGuard, RecordWriteGuard};

use crate::error::{CaError, CaResult};
use crate::runtime::sync::RwLock;
use std::collections::{BTreeSet, HashMap};
use std::sync::Arc;

use crate::server::pv::ProcessVariable;
use crate::server::record::{Record, RecordInstance, ScanType};
use crate::types::EpicsValue;

/// Parse a PV name into (base_name, field_name).
/// "TEMP.EGU" → ("TEMP", "EGU")
/// "TEMP"     → ("TEMP", "VAL")
pub fn parse_pv_name(name: &str) -> (&str, &str) {
    match name.rsplit_once('.') {
        Some((base, field)) => (base, field),
        None => (name, "VAL"),
    }
}

/// Apply timestamp to a record based on its TSE field.
/// `is_soft` indicates a Soft Channel device type.
///
/// Mirrors C `recGblGetTimeStampSimm` (recGbl.c:310-343). The TSE
/// constants are defined in `epicsTime.h:102-104`:
///
///   - `epicsTimeEventCurrentTime = 0` → wall-clock now
///   - `epicsTimeEventBestTime    = -1` → generalTime BestTime providers
///   - `epicsTimeEventDeviceTime  = -2` → device support already set time
///   - `1..` → event-number providers
///
/// The C path is symmetric: every non-`-2` case unconditionally
/// overwrites `precord->time` via `epicsTimeGetEvent(tse)`, which
/// delegates to `epicsTimeGetCurrent` for `tse==0` and to
/// `generalTimeGetEventPriority` otherwise. Only `-2` (device time)
/// is left untouched because the device support has already written
/// the timestamp before `recGblGetTimeStamp` is called.
fn apply_timestamp(common: &mut super::record::CommonFields, _is_soft: bool) {
    // Single owner of TSE -> TIME resolution; device support that must
    // format the record's resolved time during `read()` routes through the
    // same helper so the two never drift (see `recgbl::get_time_stamp`).
    // For TSE=-2 the helper returns `common.time` unchanged, preserving the
    // device-time "leave it alone" semantics.
    common.time = crate::server::recgbl::get_time_stamp(common.tse, common.time);
}

/// Unified entry in the PV database.
pub enum PvEntry {
    Simple(Arc<ProcessVariable>),
    Record(Arc<RwLock<RecordInstance>>),
}

/// Callback for resolving external PV names (CA/PVA links).
/// Returns the current value of the external PV, or None if unavailable.
pub type ExternalPvResolver = Arc<dyn Fn(&str) -> Option<EpicsValue> + Send + Sync>;

/// Async hook invoked by [`PvDatabase::has_name`] when a name is not yet
/// in the database. Used by the CA gateway and similar proxy components
/// to lazily populate PVs on first search.
///
/// The resolver should:
/// 1. Determine whether the name should be served (e.g., check ACL)
/// 2. Take whatever action is needed to make `has_name` return true on
///    a subsequent call (e.g., subscribe to an upstream IOC and call
///    `add_pv` with a placeholder value)
/// 3. Return `true` if the name is now resolvable, `false` otherwise
///
/// Returning `true` causes `has_name` to re-check the database. The
/// resolver may take some time (TCP search, upstream connect handshake);
/// the caller (UDP search responder, TCP CREATE_CHANNEL handler) will
/// `.await` it.
/// The second argument is the downstream client's socket address when
/// the lookup originates from a CA/PVA search or channel-create on
/// behalf of an identified peer (`None` for host-less internal lookups:
/// preload, iocsh, link processing). the CA gateway needs
/// this to evaluate `.pvlist` `DENY FROM host` rules at search time, the
/// way C ca-gateway's `pvExistTest` passes the client host to
/// `gateAs::findEntry`.
pub type SearchResolver = Arc<
    dyn Fn(
            String,
            Option<std::net::SocketAddr>,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = bool> + Send>>
        + Send
        + Sync,
>;

/// Per-request admission gate for an **already-registered** simple PV.
///
/// A plain IOC's simple PVs are authoritative: once registered they
/// exist unconditionally, so no gate is installed and the cached-PV
/// short-circuit in [`PvDatabase::find_entry_from`] /
/// [`PvDatabase::has_name_from`] is unchanged. A CA gateway is
/// different — its shadow PVs are projections of an upstream that can be
/// host-denied for a given requester or disconnected — so it installs a
/// gate that the lookup path consults *before* returning a cached simple
/// PV. Returning `false` makes the database answer "does not exist" for
/// that requester, exactly as C ca-gateway's `pvExistTest` returns
/// `pverDoesNotExistHere` for a host-denied or disconnected PV
/// (`gateServer.cc:1516-1637`) — without removing the PV object, so its
/// cached value stays available for diagnostics and re-admission.
///
/// The first argument is the filter-suffix-stripped record path (the
/// same key the simple-PV map and the gateway cache use); the second is
/// the requesting peer (`None` for host-less internal lookups). The gate
/// governs **only** simple PVs — records and aliases are never
/// gateway-managed and bypass it.
pub type ExistenceGate = Arc<
    dyn Fn(
            String,
            Option<std::net::SocketAddr>,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = bool> + Send>>
        + Send
        + Sync,
>;

/// Internal state of [`PvDatabase`].
///
/// # Invariant — alias-aware lookup (epics-base PR #336)
///
/// **MUST**: every record-name lookup that originates from an
/// external API (CA/PVA server, link processing, iocsh, bridge
/// providers) MUST go through [`PvDatabase::get_record`] /
/// [`PvDatabase::find_entry`] / [`PvDatabase::has_name`], never
/// `inner.records.read().await.get(...)` directly.
///
/// **MUST NOT**: a function that takes an arbitrary record-name
/// `&str` and reads `inner.records` directly, unless one of:
/// - the function is itself an alias-management primitive
///   (`add_record`, `remove_record`, `add_alias`,
///   `find_entry_no_resolve`, `has_name_no_resolve`,
///   `get_record_no_resolve`, `all_record_names`), OR
/// - the name has been normalised to canonical earlier in the
///   same scope (the `let canonical_owned; let name: &str = ...`
///   pattern in `process_record_with_links_inner` /
///   `complete_async_record_inner` / `put_record_field_from_ca` /
///   `put_pv`).
///
/// **Owner/Gate:** `PvDatabase::get_record` (alias-aware path).
///
/// New code that adds a record-name entry point should call
/// `get_record` first OR run the canonical-normalisation snippet
/// at function entry. Direct `inner.records` access is reserved
/// for the alias-management primitives listed above.
/// One CP/CPP edge in the [`PvDatabaseInner::cp_links`] index: the record
/// to (re)process when the source record changes.
///
/// `passive_only` distinguishes CPP from CP. C adds the `CA_DBPROCESS`
/// action for a CP link unconditionally, but for a CPP link only when the
/// link-holding record's `SCAN` is Passive (`dbCa.c:854,994,1072`). CP
/// edges clear the flag; CPP edges set it, and `dispatch_cp_targets`
/// honours it.
#[derive(Clone, Debug)]
pub struct CpTarget {
    pub record: String,
    pub passive_only: bool,
}

struct PvDatabaseInner {
    simple_pvs: RwLock<HashMap<String, Arc<ProcessVariable>>>,
    records: RwLock<HashMap<String, Arc<RwLock<RecordInstance>>>>,
    /// Scan index: maps scan type → sorted set of
    /// `(PHAS, load_order, record_name)`.
    ///
    /// C parity (`dbScan.c:1052-1095`): `buildScanLists` walks records
    /// in database / record-type **load order** and `addToList`
    /// inserts each after the last element with `phas <= precord->phas`
    /// — so within one PHAS value the scan list is a stable FIFO in
    /// load order. The secondary sort key is the per-record
    /// `load_order` sequence (NOT the record name), so two records
    /// sharing a PHAS scan in the order they were loaded, matching a
    /// C IOC built from the same `.db` file.
    scan_index: RwLock<HashMap<ScanType, BTreeSet<(i16, u64, String)>>>,
    /// Per-record load-order sequence number, assigned monotonically
    /// at `add_record`. Used as the secondary scan-index sort key so
    /// same-PHAS records preserve database load order. Survives a
    /// `remove_record` + re-`add_record` (the re-add gets a fresh,
    /// higher sequence — matching a fresh `.db` reload).
    load_order: RwLock<HashMap<String, u64>>,
    /// Monotonic counter feeding `load_order`.
    load_order_counter: std::sync::atomic::AtomicU64,
    /// CP/CPP link index: maps source_record → target edges to process when
    /// the source changes. Each edge carries the CP-vs-CPP distinction (see
    /// [`CpTarget`]).
    cp_links: RwLock<HashMap<String, Vec<CpTarget>>>,
    /// External (CA/PVA) CP/CPP link index: maps the *external PV name*
    /// (the cross-IOC source, e.g. `OTHER:PV` from `INP="OTHER:PV CP CA"`)
    /// → holder edges to process when that remote PV changes. The local
    /// [`Self::cp_links`] index is keyed by a local source RECORD that
    /// processes here; a cross-IOC source never processes locally, so its
    /// only trigger is the calink/pvalink CA monitor callback, which calls
    /// [`PvDatabase::dispatch_external_cp_targets`]. Parity with C
    /// `dbCa.c:993-994` `eventCallback` adding `CA_DBPROCESS`.
    external_cp_links: RwLock<HashMap<String, Vec<CpTarget>>>,
    /// Alias map: alternate-name → real-record-name. Mirrors epics-base
    /// PR #336 (alias name validation + parsing). `find_entry` and
    /// related lookups consult this map after the canonical record
    /// table so an alias resolves transparently to its target.
    aliases: RwLock<HashMap<String, String>>,
    /// Single gate that serializes
    /// every `add_pv` / `add_pv_with_hook` / `add_record` /
    /// `add_alias` / `remove_record` / `remove_simple_pv` /
    /// `remove_alias`. Without this, the per-method write-lock
    /// orders (`simple_pvs` first vs. `records` first vs.
    /// `aliases` first) could deadlock under concurrent registrations,
    /// and `add_record`'s post-insert `scan_index.write()` had a
    /// TOCTOU window where `remove_record` could land between the
    /// records map insert and the scan-index insert and leave a
    /// phantom scan entry.
    ///
    /// Holding this mutex makes the cross-namespace `check_name_free`
    /// peek atomic with the target-map insert, eliminates the
    /// scan-index race, and lets `remove_*` purge dangling aliases
    /// without a second pass.
    registration_mutex: tokio::sync::Mutex<()>,
    /// Lines queued by the iocsh `afterIocRunning <command>` directive
    /// (epics-base PR #558). Drained by the IOC application after PINI
    /// completes, then re-executed through a fresh IocShell so the
    /// commands run with the database in its post-init state.
    after_ioc_running: std::sync::Mutex<Vec<String>>,
    /// Optional resolver for external PVs (ca://, pva:// links).
    external_resolver: RwLock<Option<ExternalPvResolver>>,
    /// Optional async resolver invoked on `has_name` misses (e.g. CA gateway).
    search_resolver: RwLock<Option<SearchResolver>>,
    /// Optional per-request gate consulted before a *cached* simple PV is
    /// advertised as existing (e.g. CA gateway host/state admission). See
    /// [`ExistenceGate`]. `None` for a plain IOC (short-circuit unchanged).
    existence_gate: RwLock<Option<ExistenceGate>>,
    /// Per-scheme link sets — pluggable backends for `pva://` /
    /// `ca://` link resolution. Consulted before the legacy
    /// [`ExternalPvResolver`] in [`Self::resolve_external_pv`].
    /// Mirrors the C-EPICS lset abstraction.
    link_sets: RwLock<link_set::LinkSetRegistry>,
    /// True once the ScanScheduler has been started for this DB.
    /// Prevents duplicate scan tasks when multiple protocol servers (CA + PVA)
    /// both try to start scanning on the same DB.
    scan_started: std::sync::atomic::AtomicBool,
    /// True once PINI processing has completed. Non-owner schedulers await
    /// this before running their hooks, preserving the "PINI before hooks"
    /// ordering contract.
    pini_done: std::sync::atomic::AtomicBool,
    /// Fired by the scan owner after PINI completes. Non-owners register
    /// interest on this before re-checking `pini_done` to avoid missing the
    /// signal (`notify_waiters` does not store a permit).
    pini_notify: tokio::sync::Notify,
    /// Per-record advisory write gates — the Rust
    /// counterpart of the C-EPICS `dbScanLock` / `dbLocker`
    /// machinery. Every plain CA/PVA write, the QSRV atomic group
    /// PUT/GET, and the pvalink atomic scan-on-update epoch all
    /// acquire these gates, so no two of them can interleave on a
    /// shared record. See [`record_lock`].
    record_locks: record_lock::RecordLockRegistry,
}

/// Database of all process variables hosted by this server.
#[derive(Clone)]
pub struct PvDatabase {
    inner: Arc<PvDatabaseInner>,
}

/// Which record kind a SELM link selection is being computed for.
/// The Specified/Mask base differs between record types in C, so the
/// shared selector must know the caller.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum SelmKind {
    /// `fanout` / `seq`: Specified index is `SELN + OFFS` (0-based over
    /// LNK0..LNKF / group 0..15); Mask is shifted by `SHFT`.
    /// Mirrors `fanoutRecord.c:106-141` and `seqRecord.c:147-178`.
    FanoutSeq,
    /// `dfanout`: Specified index is `SELN - 1` (1-based, `SELN==0`
    /// means "drive nothing", `SELN > OUT_ARG_MAX` is invalid); Mask
    /// has NO `SHFT` and `SELN==0` means "no output".
    /// Mirrors `dfanoutRecord.c:307-339`.
    Dfanout,
}

/// Result of resolving a SELM/SELN selection.
#[derive(Clone, Debug, Default)]
pub(crate) struct SelmResult {
    /// 0-based link indices to drive (into the LNK0../OUTA.. array).
    pub indices: Vec<usize>,
    /// `Some` when C would raise an alarm for an out-of-range
    /// `SELN`/`OFFS`/`SHFT`. C uses `recGblSetSevr(prec, SOFT_ALARM,
    /// INVALID_ALARM)` in every such path.
    pub alarm: Option<(u16, crate::server::record::AlarmSeverity)>,
}

/// Convert a link value to `epicsUInt16` with C `dbGetLink(.., DBR_USHORT,
/// ..)` cast semantics. The dbConvert GET macro stores `*pdst =
/// (epicsUInt16) *psrc` (`dbConvert.c:63-70`) — a C cast that truncates
/// toward zero then wraps modulo 2^16, NOT a clamp. So `-1` becomes
/// `65535` and `65536` becomes `0`. Used for fanout/dfanout/seq
/// `SELL`→`SELN` so a constant, DB, CA, or PVA link source all convert
/// by the one rule C applies through `dbFastGetConvertRoutine`.
pub(crate) fn dbr_ushort_cast(value: &EpicsValue) -> u16 {
    (value.to_f64().unwrap_or(0.0) as i64) as u16
}

/// Select which link indices are active based on SELM/SELN, applying
/// the record-type-specific `OFFS`/`SHFT` bias.
///
/// SELM: 0 = All, 1 = Specified, 2 = Mask. `count` is the number of
/// link slots (16 for fanout/dfanout/seq).
///
/// `seln` is the native `DBF_USHORT` value: C declares `SELN` as
/// `epicsUInt16`, so every comparison below is unsigned, matching C's
/// selection arithmetic. A `SELL=-1` link read therefore selects
/// `65535` (out of range → INVALID for Specified, all-bits for Mask),
/// not `-1`.
///
/// C references:
/// * fanout — `fanoutRecord.c:106-141`
/// * dfanout — `dfanoutRecord.c:307-339`
/// * seq — `seqRecord.c:147-178`
pub(crate) fn select_link_indices_ex(
    kind: SelmKind,
    selm: i16,
    seln: u16,
    offs: i16,
    shft: i16,
    count: usize,
) -> SelmResult {
    use crate::server::recgbl::alarm_status::SOFT_ALARM;
    use crate::server::record::AlarmSeverity;

    let invalid = || SelmResult {
        indices: Vec::new(),
        alarm: Some((SOFT_ALARM, AlarmSeverity::Invalid)),
    };
    let ok = |indices: Vec<usize>| SelmResult {
        indices,
        alarm: None,
    };

    match selm {
        // All — every slot.
        0 => ok((0..count).collect()),
        // Specified.
        1 => match kind {
            SelmKind::FanoutSeq => {
                // C: `i = seln + offs;` with `seln` unsigned (epicsUInt16),
                // 0-based; `i<0 || i>=NLINKS` → INVALID. So `SELN=65535`
                // (from `SELL=-1`) yields `i>=NLINKS` → INVALID, never
                // drives link 0.
                let i = seln as i32 + offs as i32;
                if i < 0 || i >= count as i32 {
                    invalid()
                } else {
                    ok(vec![i as usize])
                }
            }
            SelmKind::Dfanout => {
                // C `dfanoutRecord.c:315-320`: `if (prec->seln > OUT_ARG_MAX)`
                // with `seln` unsigned → INVALID; `seln == 0` → no output;
                // otherwise drive `seln - 1`. OFFS is not a dfanout field.
                // `SELL=-1` → `SELN=65535` > count → INVALID (the signed
                // read used to see `-1`, take the `<= 0` branch, and drive
                // nothing with no alarm).
                let seln_i = seln as i32;
                if seln_i > count as i32 {
                    invalid()
                } else if seln_i == 0 {
                    ok(Vec::new())
                } else {
                    ok(vec![(seln_i - 1) as usize])
                }
            }
        },
        // Mask.
        2 => {
            let mask: u32 = match kind {
                SelmKind::FanoutSeq => {
                    // C: SHFT shift first, with `shft` range-checked to [-15,15].
                    if !(-15..=15).contains(&shft) {
                        return invalid();
                    }
                    let raw = seln as u32;
                    if shft >= 0 {
                        raw >> shft
                    } else {
                        raw << (-shft)
                    }
                }
                // dfanout Mask has no SHFT.
                SelmKind::Dfanout => seln as u32,
            };
            ok((0..count).filter(|i| mask & (1 << i) != 0).collect())
        }
        // Any other SELM value → C `default:` raises INVALID.
        _ => invalid(),
    }
}

impl PvDatabase {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(PvDatabaseInner {
                simple_pvs: RwLock::new(HashMap::new()),
                external_resolver: RwLock::new(None),
                search_resolver: RwLock::new(None),
                existence_gate: RwLock::new(None),
                link_sets: RwLock::new(link_set::LinkSetRegistry::new()),
                records: RwLock::new(HashMap::new()),
                scan_index: RwLock::new(HashMap::new()),
                load_order: RwLock::new(HashMap::new()),
                load_order_counter: std::sync::atomic::AtomicU64::new(0),
                cp_links: RwLock::new(HashMap::new()),
                external_cp_links: RwLock::new(HashMap::new()),
                aliases: RwLock::new(HashMap::new()),
                registration_mutex: tokio::sync::Mutex::new(()),
                after_ioc_running: std::sync::Mutex::new(Vec::new()),
                scan_started: std::sync::atomic::AtomicBool::new(false),
                pini_done: std::sync::atomic::AtomicBool::new(false),
                pini_notify: tokio::sync::Notify::new(),
                record_locks: record_lock::RecordLockRegistry::default(),
            }),
        }
    }

    /// Atomically claim the right to start the scan scheduler for this DB.
    /// Returns `true` on the first call, `false` on subsequent calls.
    /// Used by `ScanScheduler::run_with_hooks` to prevent duplicate scan tasks
    /// when multiple protocol servers (CA + PVA) both try to start scanning.
    pub fn try_claim_scan_start(&self) -> bool {
        self.inner
            .scan_started
            .compare_exchange(
                false,
                true,
                std::sync::atomic::Ordering::AcqRel,
                std::sync::atomic::Ordering::Acquire,
            )
            .is_ok()
    }

    /// Mark PINI processing complete. Wakes any non-owner scan schedulers
    /// that were waiting before running their hooks.
    pub fn mark_pini_done(&self) {
        self.inner
            .pini_done
            .store(true, std::sync::atomic::Ordering::Release);
        self.inner.pini_notify.notify_waiters();
    }

    /// Wait until the scan owner has completed PINI processing.
    /// Returns immediately if PINI has already completed.
    pub async fn wait_for_pini(&self) {
        if self
            .inner
            .pini_done
            .load(std::sync::atomic::Ordering::Acquire)
        {
            return;
        }
        // Register interest BEFORE re-checking the flag to avoid missing a
        // signal that arrives between the load and the await — `notify_waiters`
        // does not store a permit for late subscribers.
        let notified = self.inner.pini_notify.notified();
        if self
            .inner
            .pini_done
            .load(std::sync::atomic::Ordering::Acquire)
        {
            return;
        }
        notified.await;
    }

    /// Install an async resolver invoked when [`PvDatabase::has_name`]
    /// fails to find a name. Used by proxy/gateway implementations to
    /// lazily populate PVs on first search.
    pub async fn set_search_resolver(&self, resolver: SearchResolver) {
        *self.inner.search_resolver.write().await = Some(resolver);
    }

    /// Remove the previously installed search resolver, if any.
    pub async fn clear_search_resolver(&self) {
        *self.inner.search_resolver.write().await = None;
    }

    /// Install the per-request existence gate (see [`ExistenceGate`]).
    /// Replaces any previously installed gate. Used by the CA gateway so
    /// a cached shadow PV re-runs host/state admission per request.
    pub async fn set_existence_gate(&self, gate: ExistenceGate) {
        *self.inner.existence_gate.write().await = Some(gate);
    }

    /// Remove the previously installed existence gate, if any.
    pub async fn clear_existence_gate(&self) {
        *self.inner.existence_gate.write().await = None;
    }

    /// True when a cached simple PV named `name` must be treated as
    /// non-existent for `peer` because the installed [`ExistenceGate`]
    /// denied it. Always `false` when no gate is installed (a plain IOC)
    /// or when `name` does not resolve to a simple PV — records and
    /// aliases are never gateway-managed and bypass the gate.
    ///
    /// The single consultation point for the gate, shared by
    /// [`Self::find_entry_from`] and [`Self::has_name_from`] so the
    /// "cached simple PV ⇒ exists" short-circuit is closed uniformly on
    /// both the create and search paths.
    async fn simple_pv_gate_denies(&self, name: &str, peer: Option<std::net::SocketAddr>) -> bool {
        let gate = match self.inner.existence_gate.read().await.clone() {
            Some(g) => g,
            None => return false,
        };
        // Strip the channel-filter suffix exactly as the lookups do
        // (CA-FR-8) so the gate sees the same record-path key the
        // simple-PV map and the gateway cache are keyed on.
        let record_path = filters::split_channel_name(name).record_path;
        if !self
            .inner
            .simple_pvs
            .read()
            .await
            .contains_key(record_path.as_str())
        {
            return false;
        }
        !gate(record_path, peer).await
    }

    /// Set an external PV resolver for CA/PVA link resolution.
    /// The resolver is called synchronously from link reads.
    pub async fn set_external_resolver(&self, resolver: ExternalPvResolver) {
        *self.inner.external_resolver.write().await = Some(resolver);
    }

    /// Register a [`LinkSet`] under `scheme` (e.g. `"pva"` /
    /// `"ca"`). The lset is consulted for `ParsedLink::Pva` /
    /// `ParsedLink::Ca` link reads/writes before falling back to
    /// the legacy [`ExternalPvResolver`]. Subsequent calls for the
    /// same scheme replace the previous binding.
    pub async fn register_link_set(&self, scheme: &str, lset: link_set::DynLinkSet) {
        self.inner.link_sets.write().await.register(scheme, lset);
    }

    /// Look up the lset for `scheme`, if any.
    pub async fn link_set(&self, scheme: &str) -> Option<link_set::DynLinkSet> {
        self.inner.link_sets.read().await.get(scheme)
    }

    /// Snapshot of every registered scheme name. Stable order for
    /// `dbpvxr` dumps.
    pub async fn registered_link_schemes(&self) -> Vec<String> {
        let mut s = self.inner.link_sets.read().await.schemes();
        s.sort();
        s
    }

    /// Wait for the CA links to local records to report
    /// `is_connected() == true`. Mirrors `dbCa: iocInit wait for local CA
    /// links to connect` (epics-base PR #768/#856). The working set is
    /// exactly [`Self::external_link_targets`]: only the CA facility's
    /// local-target links — `pva://` links and non-local CA links connect
    /// in the background and are never waited on (pvxs parity).
    ///
    /// Polls every 100 ms. Returns:
    /// * `Ok(connected_count)` — the number of links that ended up
    ///   connected. May be smaller than the total when the timeout
    ///   expired before everyone was ready.
    /// * The total link count — i.e. the size of the working set
    ///   that was checked. `(connected, total)` lets the caller log
    ///   "M/N CA links connected".
    ///
    /// Pure no-op when no CA link set is registered, or when its
    /// `link_names()` has no local-target link yet (e.g. lazy-open lsets
    /// that haven't observed any record link — record processing creates
    /// the entries on first read, after iocInit returns).
    pub async fn wait_for_external_links(&self, timeout: std::time::Duration) -> (usize, usize) {
        // Collect (lset, name) pairs once. `link_names()` may grow
        // as record processing opens new links, but iocInit's wait
        // is bounded by the records loaded *before* Phase 3 — every
        // such link is already opened by the time wire_device_support
        // and setup_cp_links return.
        let targets = self.external_link_targets().await;
        let total = targets.len();
        if total == 0 {
            return (0, 0);
        }
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let connected = targets
                .iter()
                .filter(|(lset, name)| lset.is_connected(name))
                .count();
            if connected == total {
                return (connected, total);
            }
            if tokio::time::Instant::now() >= deadline {
                return (connected, total);
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
    }

    /// Snapshot the `(lset, link_name)` pairs the iocInit external-link
    /// wait reasons over. Shared by [`Self::wait_for_external_links`] and
    /// [`Self::unconnected_external_links`] so both see the identical
    /// working set.
    ///
    /// C parity: the iocInit connection-wait is a property of the CA link
    /// facility (dbCa) alone. `dbCaRun` (dbCa.c:370-380) blocks on
    /// `initOutstanding`, the count of CA links flagged
    /// `DBCA_CALLBACK_INIT_WAIT` — set only for a CA link whose target is
    /// a LOCAL record (dbLink.c:128-130):
    ///   int isLocal = dbChannelTest(pvname) == 0;
    ///   dbCaAddLinkCallbackOpt(..., isLocal ? DBCA_CALLBACK_INIT_WAIT : 0)
    /// No other external facility waits: pvxs pvalink's `linkGlobal_t::init`
    /// (ioc/pvalink.cpp) only calls `chan->open()` per channel — it opens
    /// in the background and never blocks iocInit. So the wait targets
    /// exactly the CA link set's local-target links; a non-local CA link
    /// (e.g. areaDetector's `ShutterStatusEPICS_RBV.INP = "test CP MS"`
    /// placeholder) and every `pva://` link connect asynchronously and are
    /// never held by iocInit, like C.
    async fn external_link_targets(&self) -> Vec<(link_set::DynLinkSet, String)> {
        // Only the CA facility participates — look it up directly rather
        // than iterating every registered scheme. `has_name_no_resolve`
        // is the `dbChannelTest` twin (target is a local record).
        let Some(ca_lset) = ({
            let registry = self.inner.link_sets.read().await;
            registry.get("ca")
        }) else {
            return Vec::new();
        };
        let mut targets: Vec<(link_set::DynLinkSet, String)> = Vec::new();
        for n in ca_lset.link_names() {
            if self.has_name_no_resolve(&n).await {
                targets.push((ca_lset.clone(), n));
            }
        }
        targets
    }

    /// Names of the waited-on CA links (local-target, per
    /// [`Self::external_link_targets`]) that are opened but not yet
    /// connected. iocInit calls this after
    /// [`Self::wait_for_external_links`] times out so the
    /// "M/N connected" diagnostic can name the `N-M` it proceeded
    /// without, instead of leaving the operator to run `dbcar`.
    /// `pva://` links are not in this set — they never block iocInit.
    pub async fn unconnected_external_links(&self) -> Vec<String> {
        self.external_link_targets()
            .await
            .into_iter()
            .filter(|(lset, name)| !lset.is_connected(name))
            .map(|(_, name)| name)
            .collect()
    }

    /// Enumerate every link-shaped field on `record_name`. Returns
    /// `(field_name, link_string, parsed)` tuples for fields whose
    /// raw value parses as a non-trivial link via
    /// [`crate::server::record::parse_link_v2`]. Used by `dbpvxr` to
    /// dump per-record link state without hardcoding the field-name
    /// list — works across record types as long as they expose link
    /// strings via [`Record::get_field`].
    ///
    /// Returns an empty Vec when the record doesn't exist.
    pub async fn record_link_fields(
        &self,
        record_name: &str,
    ) -> Vec<(String, String, crate::server::record::ParsedLink)> {
        let rec = match self.get_record(record_name).await {
            Some(r) => r,
            None => return Vec::new(),
        };
        let inst = rec.read().await;
        let mut out = Vec::new();
        let push = |field: &str, raw: &str, out: &mut Vec<_>| {
            if raw.is_empty() {
                return;
            }
            let parsed = crate::server::record::parse_link_v2(raw);
            if !matches!(parsed, crate::server::record::ParsedLink::None) {
                out.push((field.to_string(), raw.to_string(), parsed));
            }
        };
        // Canonical link-bearing fields stored on `CommonFields` as raw
        // String. These do NOT appear as `DbFieldType::String` entries in
        // `field_list()`: an `ai`'s `INP` / an `ao`'s `OUT` carry
        // `DBF_INLINK` / `DBF_OUTLINK` descriptors (and `INP`/`OUT` are
        // not in the record's static field table at all), so the previous
        // `field_list()` scan filtered by `String` silently dropped every
        // device-support link — the holder's pvalink monitor was never
        // opened. Enumerate the canonical storage directly so this method
        // is the single owner of "which fields on a record are links",
        // shared by `setup_cp_links` (CA CP/CPP) and the pvalink install
        // scan (PVA CP/CPP).
        push("INP", &inst.common.inp, &mut out);
        push("OUT", &inst.common.out, &mut out);
        push("TSEL", &inst.common.tsel, &mut out);
        push("SDIS", &inst.common.sdis, &mut out);
        // Record-specific multi-input links (INPA..INPL for
        // calc/calcout/sel/sub) and the CP-capable input link fields
        // (DOL family, NVL, SELL, SGNL).
        let mut field_names: Vec<&str> = inst
            .record
            .multi_input_links()
            .iter()
            .map(|(lf, _vf)| *lf)
            .collect();
        field_names.extend_from_slice(crate::server::database::links::CP_INPUT_LINK_FIELDS);
        for field in field_names {
            if let Some(EpicsValue::String(s)) = inst.record.get_field(field) {
                push(field, &s.as_str_lossy(), &mut out);
            }
        }
        out
    }

    /// Resolve an external PV name. Dispatches through the
    /// `(scheme, name)` lset if one is registered; otherwise falls
    /// back to the legacy [`ExternalPvResolver`] closure. `name`
    /// may be the bare PV name (in which case `pva://` is assumed
    /// when an lset is registered for that scheme) or a fully
    /// scheme-prefixed string.
    pub(crate) async fn resolve_external_pv(&self, name: &str) -> Option<EpicsValue> {
        // Try lsets first. We accept both "scheme://body" and the
        // bare body (stored in ParsedLink::Pva/Ca after the
        // dispatch in record/link.rs).
        let (scheme, body) = if let Some(rest) = name.strip_prefix("pva://") {
            ("pva", rest)
        } else if let Some(rest) = name.strip_prefix("ca://") {
            ("ca", rest)
        } else {
            // No prefix — try every registered lset in turn. The
            // first one with a value for `name` wins. Schemes are
            // single-digit so this is cheap.
            let registry = self.inner.link_sets.read().await;
            for s in registry.schemes() {
                if let Some(lset) = registry.get(&s) {
                    if let Some(v) = lset.get_value(name) {
                        return Some(v);
                    }
                }
            }
            drop(registry);
            // Fall through to legacy resolver.
            let resolver = self.inner.external_resolver.read().await;
            return resolver.as_ref().and_then(|r| r(name));
        };
        if let Some(lset) = self.inner.link_sets.read().await.get(scheme) {
            if let Some(v) = lset.get_value(body) {
                return Some(v);
            }
        }
        let resolver = self.inner.external_resolver.read().await;
        resolver.as_ref().and_then(|r| r(name))
    }

    /// Add a simple PV with an initial value.
    ///
    /// Returns `Err` when `name` is already registered as a simple PV,
    /// a record, or an alias — mirroring epics-base C IOC which treats
    /// duplicate `dbLoadRecords` names as a fatal error. Callers that
    /// want replace-on-overwrite semantics must first call
    /// `remove_simple_pv` / `remove_record` / `remove_alias`.
    ///
    /// Serialized through `registration_mutex` so the
    /// cross-namespace check is atomic with the insert and the lock
    /// order across all add_*/remove_* methods is identical (no
    /// cross-namespace deadlock).
    pub async fn add_pv(&self, name: &str, initial: EpicsValue) -> CaResult<()> {
        let _gate = self.inner.registration_mutex.lock().await;
        self.check_name_free(name).await?;
        let pv = Arc::new(ProcessVariable::new(name.to_string(), initial));
        self.inner
            .simple_pvs
            .write()
            .await
            .insert(name.to_string(), pv);
        Ok(())
    }

    /// Add a simple PV that already has a [`WriteHook`] installed.
    ///
    /// Equivalent to `add_pv` followed by `find_pv` + `set_write_hook`,
    /// but the PV is constructed with the hook in place so it is
    /// inserted into the `simple_pvs` map ATOMICALLY with the hook
    /// already attached. Closes a small race in proxy/gateway code
    /// where a downstream client could (in principle) `CREATE_CHAN` +
    /// `WRITE_NOTIFY` between the two awaits and hit the local
    /// `pv.set()` fallback path before the hook landed.
    ///
    /// Returns `Err` on duplicate name (see [`add_pv`]).
    pub async fn add_pv_with_hook(
        &self,
        name: &str,
        initial: EpicsValue,
        hook: crate::server::pv::WriteHook,
    ) -> CaResult<()> {
        self.add_pv_with_hooks(name, initial, hook, None).await
    }

    /// like [`Self::add_pv_with_hook`] but also installs an
    /// optional [`AccessHook`](crate::server::pv::AccessHook) so the CA
    /// gateway can route this shadow PV's read/write access-rights
    /// decision through its own ACF. Both hooks are attached before the
    /// PV is inserted into `simple_pvs`, so a downstream `CREATE_CHAN`
    /// cannot observe the PV without its access hook bound.
    pub async fn add_pv_with_hooks(
        &self,
        name: &str,
        initial: EpicsValue,
        write_hook: crate::server::pv::WriteHook,
        access_hook: Option<crate::server::pv::AccessHook>,
    ) -> CaResult<()> {
        self.add_pv_with_hooks_full(name, initial, write_hook, access_hook, None)
            .await
    }

    /// like [`Self::add_pv_with_hooks`] but also installs an optional
    /// [`ReadHook`](crate::server::pv::ReadHook) so a proxy (the CA
    /// gateway in no-cache mode) can serve each downstream GET from a
    /// fresh upstream fetch instead of the stored value. All three hooks
    /// are attached before the PV is inserted into `simple_pvs`, so a
    /// downstream `CREATE_CHAN` cannot observe the PV without its hooks
    /// bound — the read hook lands atomically with registration, closing
    /// the same race the write/access hooks already close. `read_hook:
    /// None` is identical to [`Self::add_pv_with_hooks`].
    pub async fn add_pv_with_hooks_full(
        &self,
        name: &str,
        initial: EpicsValue,
        write_hook: crate::server::pv::WriteHook,
        access_hook: Option<crate::server::pv::AccessHook>,
        read_hook: Option<crate::server::pv::ReadHook>,
    ) -> CaResult<()> {
        let _gate = self.inner.registration_mutex.lock().await;
        self.check_name_free(name).await?;
        let pv = Arc::new(ProcessVariable::new(name.to_string(), initial));
        pv.set_write_hook(write_hook);
        if let Some(access) = access_hook {
            pv.set_access_hook(access);
        }
        if let Some(read) = read_hook {
            pv.set_read_hook(read);
        }
        self.inner
            .simple_pvs
            .write()
            .await
            .insert(name.to_string(), pv);
        Ok(())
    }

    /// Remove a simple PV by name. Returns `Some(pv)` if a PV was
    /// removed. Used by the gateway sweep so an evicted upstream
    /// subscription doesn't leave a stale shadow PV (with a now-dead
    /// `WriteHook` capturing an aborted upstream channel).
    ///
    /// Also purges any aliases that pointed AT this name
    /// (otherwise a re-add of the same alias name would fail with
    /// "already registered as an alias" even though its target is
    /// gone).
    pub async fn remove_simple_pv(&self, name: &str) -> Option<Arc<ProcessVariable>> {
        let _gate = self.inner.registration_mutex.lock().await;
        // Simple PVs cannot be alias targets (aliases point at
        // records), but a stale alias whose name MATCHES this PV
        // would have been rejected at add_alias time. No alias
        // cleanup needed for simple-PV removal.
        self.inner.simple_pvs.write().await.remove(name)
    }

    /// Add a record (accepts a boxed Record to avoid double-boxing).
    ///
    /// Returns `Err` when `name` collides with an existing record,
    /// simple PV, or alias. The C IOC's `dbLoadRecords` treats this as
    /// fatal; do not silently replace.
    ///
    /// The records-map insert AND scan-index insert run
    /// under the same `registration_mutex` hold, eliminating the
    /// TOCTOU window where `remove_record` could land between them
    /// and leave a phantom scan entry.
    pub async fn add_record(&self, name: &str, record: Box<dyn Record>) -> CaResult<()> {
        let _gate = self.inner.registration_mutex.lock().await;
        self.check_name_free(name).await?;
        let mut instance = RecordInstance::new_boxed(name.to_string(), record);
        // Hand the record a cycle-free handle to its own database so it can
        // post out-of-band field updates / wire completion-driven re-entry
        // (asyn TRACE callback, sseq WAITn) without owning the database.
        // C records reach `dbCommon::pdba`/the IOC the same way at
        // `dbDefineRecord` init; the framework supplies the back-reference,
        // the record never constructs it. Defaulted no-op for records that
        // do not need it.
        instance
            .record
            .set_async_context(name.to_string(), self.async_handle());
        let scan = instance.common.scan;
        let phas = instance.common.phas;
        self.inner
            .records
            .write()
            .await
            .insert(name.to_string(), Arc::new(RwLock::new(instance)));

        // Assign a monotonic load-order sequence — the scan-index
        // secondary sort key, so same-PHAS records keep load order.
        let seq = self
            .inner
            .load_order_counter
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.inner
            .load_order
            .write()
            .await
            .insert(name.to_string(), seq);

        if scan != ScanType::Passive {
            self.inner
                .scan_index
                .write()
                .await
                .entry(scan)
                .or_default()
                .insert((phas, seq, name.to_string()));
        }
        Ok(())
    }

    /// Verify that `name` is not currently registered in any of the
    /// three namespaces. Caller MUST hold `registration_mutex` so the
    /// peek-then-insert sequence is atomic — without that, two tasks
    /// can both see the name as free and race the insert.
    async fn check_name_free(&self, name: &str) -> CaResult<()> {
        let kind = if self.inner.simple_pvs.read().await.contains_key(name) {
            Some("simple PV")
        } else if self.inner.records.read().await.contains_key(name) {
            Some("record")
        } else if self.inner.aliases.read().await.contains_key(name) {
            Some("alias")
        } else {
            None
        };
        if let Some(kind) = kind {
            return Err(CaError::DbParseError {
                line: 0,
                column: 0,
                message: format!("name '{name}' is already registered as a {kind}"),
            });
        }
        Ok(())
    }

    /// Remove a record by name. Returns `true` if a record was removed,
    /// `false` if no such name was registered. Mirrors epics-base PR
    /// #505 — deletion at database creation, exposed here as a public
    /// API so iocsh `dbDeleteRecord` and tests can drive it.
    ///
    /// The cleanup covers the three indices that `add_record` populates:
    /// the records map, the scan index, and CP-link source/target lists.
    /// Live subscribers on the removed record drop their `Sender` clone
    /// when the `RecordInstance` is dropped — they observe `Closed` on
    /// next recv, matching the existing dbEvent cancel flow.
    pub async fn remove_record(&self, name: &str) -> bool {
        let _gate = self.inner.registration_mutex.lock().await;
        // 1) Remove from main map; keep scan + phas for scan-index cleanup.
        let removed = self.inner.records.write().await.remove(name);
        let Some(rec_arc) = removed else {
            return false;
        };
        let scan = {
            let inst = rec_arc.read().await;
            inst.common.scan
        };

        // 2) Drop from scan index if it was scheduled. Match by record
        // name only — PHAS and load_order are not needed and may be
        // stale relative to the entry actually present.
        if scan != ScanType::Passive {
            let mut idx = self.inner.scan_index.write().await;
            if let Some(set) = idx.get_mut(&scan) {
                set.retain(|(_, _, n)| n != name);
                if set.is_empty() {
                    idx.remove(&scan);
                }
            }
        }

        // 2b) Drop the load-order entry.
        self.inner.load_order.write().await.remove(name);

        // 3) Drop from CP-link tables. Removed both as source (channel
        // change → trigger targets) and as target (other channels'
        // CP lists may still reference this name).
        let mut cp = self.inner.cp_links.write().await;
        cp.remove(name);
        for targets in cp.values_mut() {
            targets.retain(|t| t.record != name);
        }
        drop(cp);

        // 4) Purge aliases that pointed AT the
        // removed record. Otherwise `find_pv("ALT")` returns None
        // (target gone) but `add_pv("ALT", ...)` still fails with
        // "already registered as an alias" — orphan blocks reuse.
        let mut aliases = self.inner.aliases.write().await;
        aliases.retain(|_alias, target| target != name);

        true
    }

    /// Internal: synchronous lookup without invoking the search resolver.
    async fn find_entry_no_resolve(&self, name: &str) -> Option<PvEntry> {
        // a channel name may carry a `.{"arr":...}` filter
        // suffix. Strip it before lookup — the suffix is a per-channel
        // filter spec, not part of the PV identity. `split_channel_name`
        // is the single owner of "channel name → record_path" and is
        // idempotent on an already-stripped name. Without this a
        // filtered SimplePv (`SP.{"arr":...}`) never matches
        // `simple_pvs` (keyed by the bare PV name) and even a filtered
        // record fails when the JSON contains a `.` (e.g.
        // `{"dbnd":{"d":0.5}}`), because the bare `parse_pv_name` last-dot
        // split would tear the suffix apart instead of removing it.
        let record_path = filters::split_channel_name(name).record_path;
        let (base, _field) = parse_pv_name(&record_path);

        if let Some(pv) = self.inner.simple_pvs.read().await.get(record_path.as_str()) {
            return Some(PvEntry::Simple(pv.clone()));
        }
        if let Some(rec) = self.inner.records.read().await.get(base) {
            return Some(PvEntry::Record(rec.clone()));
        }
        // Alias resolve (epics-base PR #336): the alternate name maps
        // to a canonical record name. Look up the real record after
        // translating the base.
        if let Some(target) = self.inner.aliases.read().await.get(base).cloned() {
            if let Some(rec) = self.inner.records.read().await.get(&target) {
                return Some(PvEntry::Record(rec.clone()));
            }
        }
        None
    }

    /// Register an alias `alias` for an existing record `target`.
    /// Mirrors epics-base PR #336. Returns `Err(...)` when the target
    /// does not exist or the alias name is already in use anywhere
    /// in the database (records, simple PVs, or other aliases).
    ///
    /// Pre-fix the alias path checked only
    /// `records` and `aliases` — a simple-PV with the same name as
    /// the proposed alias was missed, leaving the database in a
    /// state where `find_pv(alias)` could resolve to either the
    /// simple PV or the alias-mapped record depending on lookup
    /// order. Now we run the same cross-namespace `check_name_free`
    /// guard the other add_* paths use.
    pub async fn add_alias(&self, alias: &str, target: &str) -> CaResult<()> {
        let _gate = self.inner.registration_mutex.lock().await;
        if !self.inner.records.read().await.contains_key(target) {
            return Err(CaError::ChannelNotFound(format!(
                "alias target '{target}' is not a registered record"
            )));
        }
        self.check_name_free(alias).await?;
        self.inner
            .aliases
            .write()
            .await
            .insert(alias.to_string(), target.to_string());
        Ok(())
    }

    /// Resolve an alias to its target record name, or `None` when the
    /// name is not an alias.
    pub async fn resolve_alias(&self, name: &str) -> Option<String> {
        self.inner.aliases.read().await.get(name).cloned()
    }

    /// Queue an iocsh command line for post-PINI execution.
    /// Mirrors epics-base PR #558 — `afterIocRunning <command>` lets
    /// the startup script schedule actions that run after iocInit
    /// completes (when the record set is fully wired up).
    pub fn queue_after_ioc_running(&self, line: impl Into<String>) {
        self.inner
            .after_ioc_running
            .lock()
            .unwrap()
            .push(line.into());
    }

    /// Drain the post-PINI iocsh command queue. Called by
    /// `IocApplication::run` after PINI processing.
    pub fn take_after_ioc_running(&self) -> Vec<String> {
        std::mem::take(&mut *self.inner.after_ioc_running.lock().unwrap())
    }

    /// Internal: synchronous existence check without resolver.
    async fn has_name_no_resolve(&self, name: &str) -> bool {
        // strip the channel-filter suffix before lookup so a
        // filtered channel (`SP.{"arr":...}` / `REC.{"dbnd":{"d":0.5}}`)
        // resolves to its underlying PV at UDP-search time. This is the
        // search-side twin of `find_entry_no_resolve`; without it a
        // filtered SimplePv never answers a SEARCH and the client never
        // reaches CREATE_CHAN. See that function for the full rationale.
        let record_path = filters::split_channel_name(name).record_path;
        let (base, _) = parse_pv_name(&record_path);
        if self
            .inner
            .simple_pvs
            .read()
            .await
            .contains_key(record_path.as_str())
        {
            return true;
        }
        if self.inner.records.read().await.contains_key(base) {
            return true;
        }
        // Alias entry exists and points to a live record
        // (epics-base PR #336).
        if let Some(target) = self.inner.aliases.read().await.get(base) {
            return self.inner.records.read().await.contains_key(target);
        }
        false
    }

    /// Look up an entry by name. Supports "record.FIELD" syntax.
    ///
    /// If the name is not found and a search resolver is installed,
    /// the resolver is invoked once. If the resolver returns true, the
    /// database is re-checked.
    pub async fn find_entry(&self, name: &str) -> Option<PvEntry> {
        self.find_entry_from(name, None).await
    }

    /// Like [`Self::find_entry`], but threads the downstream client's
    /// socket address into the search resolver. The CA TCP CREATE_CHANNEL
    /// handler passes the connection peer so the gateway can apply
    /// host-scoped `.pvlist` admission.
    pub async fn find_entry_from(
        &self,
        name: &str,
        peer: Option<std::net::SocketAddr>,
    ) -> Option<PvEntry> {
        if let Some(entry) = self.find_entry_no_resolve(name).await {
            // A cached simple PV must still pass the per-request
            // existence gate (CA gateway host/state admission). When the
            // gate denies it, answer does-not-exist for this requester
            // instead of returning the stale shadow entry — C ca-gateway
            // re-runs `gateAs::findEntry`/cache-state on every
            // `pvExistTest` (gateServer.cc:1516-1637). Records/aliases
            // bypass the gate (see `simple_pv_gate_denies`).
            if matches!(entry, PvEntry::Simple(_)) && self.simple_pv_gate_denies(name, peer).await {
                return None;
            }
            return Some(entry);
        }
        // Try the search resolver
        let resolver = self.inner.search_resolver.read().await.clone();
        if let Some(r) = resolver {
            if r(name.to_string(), peer).await {
                return self.find_entry_no_resolve(name).await;
            }
        }
        None
    }

    /// Check if a base name exists (for UDP search).
    ///
    /// If the name is not in the database and a search resolver is installed,
    /// the resolver is invoked. The resolver may populate the database
    /// (e.g., subscribe to an upstream IOC and add a placeholder PV) and
    /// return true; this method then re-checks.
    pub async fn has_name(&self, name: &str) -> bool {
        self.has_name_from(name, None).await
    }

    /// Like [`Self::has_name`], but threads the downstream client's
    /// socket address into the search resolver. The CA UDP search
    /// responder passes the datagram source address so the gateway can
    /// apply host-scoped `.pvlist` admission.
    pub async fn has_name_from(&self, name: &str, peer: Option<std::net::SocketAddr>) -> bool {
        if self.has_name_no_resolve(name).await {
            // Same per-request gate as `find_entry_from`: a cached simple
            // PV the gateway's host/state admission denies must answer
            // does-not-exist at search time. Records/aliases bypass.
            if self.simple_pv_gate_denies(name, peer).await {
                return false;
            }
            return true;
        }
        let resolver = self.inner.search_resolver.read().await.clone();
        if let Some(r) = resolver {
            if r(name.to_string(), peer).await {
                return self.has_name_no_resolve(name).await;
            }
        }
        false
    }

    /// Look up a simple PV by name (backward-compatible).
    pub async fn find_pv(&self, name: &str) -> Option<Arc<ProcessVariable>> {
        if let Some(pv) = self.inner.simple_pvs.read().await.get(name) {
            return Some(pv.clone());
        }
        None
    }

    /// Get a record Arc by name. Alias-aware (epics-base PR #336):
    /// when `name` is not a canonical record but matches a registered
    /// alias, the alias' target record is returned. Mirrors base
    /// `dbNameToAddr` behaviour, so dbpf/dbpr/dbgf, CA channel lookup,
    /// and DB-link target resolution all work transparently for
    /// aliases.
    ///
    /// Use [`Self::get_record_no_resolve`] when the caller already
    /// holds a canonical name and wants to suppress the alias path
    /// (e.g. to detect alias collisions during builder wiring).
    pub async fn get_record(&self, name: &str) -> Option<Arc<RwLock<RecordInstance>>> {
        if let Some(rec) = self.inner.records.read().await.get(name).cloned() {
            return Some(rec);
        }
        let target = self.inner.aliases.read().await.get(name).cloned()?;
        self.inner.records.read().await.get(&target).cloned()
    }

    /// Strict variant of [`Self::get_record`] — does NOT consult the
    /// alias table. Returns `Some` only when a canonical record with
    /// that exact name exists.
    pub async fn get_record_no_resolve(&self, name: &str) -> Option<Arc<RwLock<RecordInstance>>> {
        self.inner.records.read().await.get(name).cloned()
    }

    /// Get all record names.
    pub async fn all_record_names(&self) -> Vec<String> {
        self.inner.records.read().await.keys().cloned().collect()
    }

    /// Get all alias names registered against existing records.
    /// Mirrors the alias-half of base's `dbFirstRecord` iteration —
    /// `dbgrep` / `dbglob` / `dbsr` walk both record names and
    /// aliases when matching a glob.
    pub async fn all_alias_names(&self) -> Vec<String> {
        self.inner.aliases.read().await.keys().cloned().collect()
    }

    /// Return every alias that points at `canonical`. Sorted for
    /// stable output; empty when the record has no aliases. Used by
    /// `dbpr` to surface alias-form names so admins can see how
    /// clients may reach the record.
    pub async fn aliases_for_record(&self, canonical: &str) -> Vec<String> {
        let aliases = self.inner.aliases.read().await;
        let mut hits: Vec<String> = aliases
            .iter()
            .filter_map(|(alias, target)| {
                if target == canonical {
                    Some(alias.clone())
                } else {
                    None
                }
            })
            .collect();
        hits.sort();
        hits
    }

    /// Get all simple PV names.
    pub async fn all_simple_pv_names(&self) -> Vec<String> {
        self.inner.simple_pvs.read().await.keys().cloned().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// C `recGblGetTimeStampSimm` (recGbl.c:310-343) maps TSE values
    /// to epicsTime sources via the constants in `epicsTime.h:102-104`.
    /// The Rust port previously misread TSE=-1 as "device-provided
    /// with BestTime fallback" and gated the BestTime call on a
    /// UNIX_EPOCH check. C calls `epicsTimeGetEvent(-1)`
    /// unconditionally; only TSE=-2 (epicsTimeEventDeviceTime) leaves
    /// `precord->time` untouched.
    ///
    /// Regression: a stale device write (any non-epoch SystemTime)
    /// suppressed every BestTime refresh thereafter.
    #[test]
    fn apply_timestamp_tse_minus_one_always_overwrites_with_best_time() {
        use crate::server::record::CommonFields;
        use std::time::{Duration, SystemTime};

        // Pre-populate `time` with a stale but non-epoch sentinel.
        let stale = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
        let mut common = CommonFields::default();
        common.tse = -1;
        common.time = stale;

        apply_timestamp(&mut common, false);

        // BestTime must have run unconditionally — `common.time` is
        // no longer the stale sentinel.
        assert_ne!(
            common.time, stale,
            "TSE=-1 must always overwrite via generalTime BestTime, \
             matching C epicsTimeGetEvent(-1) called unconditionally"
        );
    }

    /// C `epicsTimeEventDeviceTime = -2` (epicsTime.h:104). The C
    /// path does NOT call `epicsTimeGetEvent` for this TSE value;
    /// device support has already set `precord->time` before the
    /// recGbl call. The Rust port must leave `common.time` untouched.
    #[test]
    fn apply_timestamp_tse_minus_two_preserves_device_provided_time() {
        use crate::server::record::CommonFields;
        use std::time::{Duration, SystemTime};

        let device_time = SystemTime::UNIX_EPOCH + Duration::from_secs(2_000_000);
        let mut common = CommonFields::default();
        common.tse = -2;
        common.time = device_time;

        apply_timestamp(&mut common, false);

        assert_eq!(
            common.time, device_time,
            "TSE=-2 (epicsTimeEventDeviceTime) must preserve device-provided time"
        );
    }

    #[test]
    fn select_link_indices_fanout_all_specified_mask() {
        use crate::server::record::AlarmSeverity;
        // All — every slot.
        let r = select_link_indices_ex(SelmKind::FanoutSeq, 0, 0, 0, 0, 16);
        assert_eq!(r.indices, (0..16).collect::<Vec<_>>());
        assert!(r.alarm.is_none());

        // Specified, 0-based: SELN=0 selects LNK0 (C parity, fanout).
        let r = select_link_indices_ex(SelmKind::FanoutSeq, 1, 0, 0, 0, 16);
        assert_eq!(r.indices, vec![0]);
        // Specified with OFFS bias: SELN=2 + OFFS=3 → index 5.
        let r = select_link_indices_ex(SelmKind::FanoutSeq, 1, 2, 3, 0, 16);
        assert_eq!(r.indices, vec![5]);
        // Out-of-range Specified → INVALID alarm, no links.
        let r = select_link_indices_ex(SelmKind::FanoutSeq, 1, 20, 0, 0, 16);
        assert!(r.indices.is_empty());
        assert_eq!(r.alarm, Some((15, AlarmSeverity::Invalid)));
        // Negative resolved index (SELN + negative OFFS) → INVALID.
        let r = select_link_indices_ex(SelmKind::FanoutSeq, 1, 0, -1, 0, 16);
        assert_eq!(r.alarm, Some((15, AlarmSeverity::Invalid)));

        // Mask: SELN=0b101 → bits 0 and 2.
        let r = select_link_indices_ex(SelmKind::FanoutSeq, 2, 5, 0, 0, 16);
        assert_eq!(r.indices, vec![0, 2]);
        // Mask with SHFT: SELN=0b101 >> 1 = 0b10 → bit 1.
        let r = select_link_indices_ex(SelmKind::FanoutSeq, 2, 5, 0, 1, 16);
        assert_eq!(r.indices, vec![1]);
        // Mask with negative SHFT: SELN=0b101 << 1 = 0b1010 → bits 1,3.
        let r = select_link_indices_ex(SelmKind::FanoutSeq, 2, 5, 0, -1, 16);
        assert_eq!(r.indices, vec![1, 3]);
        // SHFT out of [-15,15] → INVALID.
        let r = select_link_indices_ex(SelmKind::FanoutSeq, 2, 5, 0, 16, 16);
        assert_eq!(r.alarm, Some((15, AlarmSeverity::Invalid)));

        // Unknown SELM → INVALID.
        let r = select_link_indices_ex(SelmKind::FanoutSeq, 9, 0, 0, 0, 16);
        assert_eq!(r.alarm, Some((15, AlarmSeverity::Invalid)));
    }

    #[test]
    fn select_link_indices_dfanout_specified_is_one_based() {
        use crate::server::record::AlarmSeverity;
        // dfanout Specified is 1-based: SELN=1 → OUTA (index 0).
        let r = select_link_indices_ex(SelmKind::Dfanout, 1, 1, 0, 0, 16);
        assert_eq!(r.indices, vec![0]);
        // SELN=2 → OUTB (index 1).
        let r = select_link_indices_ex(SelmKind::Dfanout, 1, 2, 0, 0, 16);
        assert_eq!(r.indices, vec![1]);
        // SELN=0 → drive nothing, NO alarm.
        let r = select_link_indices_ex(SelmKind::Dfanout, 1, 0, 0, 0, 16);
        assert!(r.indices.is_empty());
        assert!(r.alarm.is_none());
        // SELN > 16 → INVALID.
        let r = select_link_indices_ex(SelmKind::Dfanout, 1, 17, 0, 0, 16);
        assert_eq!(r.alarm, Some((15, AlarmSeverity::Invalid)));
        // dfanout Mask has no SHFT — SHFT arg ignored.
        let r = select_link_indices_ex(SelmKind::Dfanout, 2, 5, 0, 7, 16);
        assert_eq!(r.indices, vec![0, 2]);
    }

    #[test]
    fn seln_is_unsigned_dbr_ushort_cast() {
        use crate::server::record::AlarmSeverity;
        // `dbr_ushort_cast` reproduces C's `(epicsUInt16)` cast
        // (dbConvert.c:63-70): truncate toward zero, wrap mod 2^16, NOT a
        // clamp. SELL=-1 → 65535, SELL=65536 → 0.
        assert_eq!(dbr_ushort_cast(&EpicsValue::Double(-1.0)), 65535);
        assert_eq!(dbr_ushort_cast(&EpicsValue::Double(65536.0)), 0);
        assert_eq!(dbr_ushort_cast(&EpicsValue::Double(3.7)), 3);
        assert_eq!(dbr_ushort_cast(&EpicsValue::Long(-1)), 65535);

        // SELL=-1 casts to the unsigned `DBF_USHORT` value 65535.
        let seln_max = 65535u16;
        // fanout/seq Specified: C `i = (epicsUInt16)seln + offs` =
        // 65535 → out of range → INVALID. The old signed read clamped to
        // 0 and drove link 0.
        let r = select_link_indices_ex(SelmKind::FanoutSeq, 1, seln_max, 0, 0, 16);
        assert!(r.indices.is_empty());
        assert_eq!(r.alarm, Some((15, AlarmSeverity::Invalid)));
        // fanout/seq Mask: 65535 → all 16 low bits set → every link. The
        // old clamp produced an empty mask.
        let r = select_link_indices_ex(SelmKind::FanoutSeq, 2, seln_max, 0, 0, 16);
        assert_eq!(r.indices, (0..16).collect::<Vec<_>>());
        // dfanout Specified: 65535 > count → INVALID. The old signed read
        // saw -1 ≤ 0 → drove nothing with no alarm.
        let r = select_link_indices_ex(SelmKind::Dfanout, 1, seln_max, 0, 0, 16);
        assert!(r.indices.is_empty());
        assert_eq!(r.alarm, Some((15, AlarmSeverity::Invalid)));
    }

    /// Lset that flips to "connected" after a configurable delay.
    /// Drives the wait_for_external_links time-budget tests below.
    struct DelayedConnectLset {
        names: Vec<String>,
        connect_at: tokio::time::Instant,
    }

    impl link_set::LinkSet for DelayedConnectLset {
        fn is_connected(&self, _: &str) -> bool {
            tokio::time::Instant::now() >= self.connect_at
        }
        fn get_value(&self, _: &str) -> Option<EpicsValue> {
            None
        }
        fn link_names(&self) -> Vec<String> {
            self.names.clone()
        }
    }

    #[tokio::test]
    async fn wait_for_external_links_returns_zero_zero_when_no_lsets() {
        let db = PvDatabase::new();
        let (c, t) = db
            .wait_for_external_links(std::time::Duration::from_millis(50))
            .await;
        assert_eq!((c, t), (0, 0));
    }

    #[tokio::test]
    async fn wait_for_external_links_connected_quickly() {
        let db = PvDatabase::new();
        // Local-target forced-CA links (dbChannelTest==0 → isLocal): these
        // get DBCA_CALLBACK_INIT_WAIT, so iocInit waits for them.
        db.add_pv("pv:A", EpicsValue::Long(0)).await.unwrap();
        db.add_pv("pv:B", EpicsValue::Long(0)).await.unwrap();
        let lset = Arc::new(DelayedConnectLset {
            names: vec!["pv:A".to_string(), "pv:B".to_string()],
            connect_at: tokio::time::Instant::now(),
        });
        // Registered under "ca": the iocInit wait is CA-facility only, so
        // the working set comes from the "ca" link set (these forced-CA
        // local-target links), never from a "pva" set.
        db.register_link_set("ca", lset).await;
        let (c, t) = db
            .wait_for_external_links(std::time::Duration::from_secs(1))
            .await;
        assert_eq!((c, t), (2, 2));
    }

    #[tokio::test]
    async fn wait_for_external_links_returns_partial_on_timeout() {
        let db = PvDatabase::new();
        // Local target so the link is in the init-wait set (dbLink.c:130);
        // connect-time well past the budget below, so the wait must return
        // (0, 1) instead of blocking.
        db.add_pv("slow:pv", EpicsValue::Long(0)).await.unwrap();
        let lset = Arc::new(DelayedConnectLset {
            names: vec!["slow:pv".to_string()],
            connect_at: tokio::time::Instant::now() + std::time::Duration::from_secs(60),
        });
        db.register_link_set("ca", lset).await;
        let started = tokio::time::Instant::now();
        let (c, t) = db
            .wait_for_external_links(std::time::Duration::from_millis(250))
            .await;
        let elapsed = started.elapsed();
        assert_eq!((c, t), (0, 1));
        assert!(
            elapsed >= std::time::Duration::from_millis(200),
            "wait must consume at least the configured budget, got {:?}",
            elapsed
        );
        assert!(
            elapsed < std::time::Duration::from_secs(2),
            "wait must not exceed the budget by much, got {:?}",
            elapsed
        );
    }

    /// C parity (dbLink.c:130): a link whose target is NOT a local record
    /// (`dbChannelTest != 0`) gets no DBCA_CALLBACK_INIT_WAIT, so iocInit
    /// must not block on it. An areaDetector `test CP MS` placeholder — a CP
    /// link to a PV that exists nowhere — must drop straight through, leaving
    /// the link to connect (or dangle) asynchronously and silently, like C.
    #[tokio::test]
    async fn wait_for_external_links_skips_nonlocal_targets() {
        let db = PvDatabase::new();
        // "test" has no local record and would never connect.
        let lset = Arc::new(DelayedConnectLset {
            names: vec!["test".to_string()],
            connect_at: tokio::time::Instant::now() + std::time::Duration::from_secs(60),
        });
        db.register_link_set("ca", lset).await;
        let started = tokio::time::Instant::now();
        let (c, t) = db
            .wait_for_external_links(std::time::Duration::from_secs(10))
            .await;
        // Non-local target is excluded from the wait set entirely, so the
        // call returns (0, 0) immediately rather than blocking the budget.
        assert_eq!((c, t), (0, 0));
        assert!(
            started.elapsed() < std::time::Duration::from_secs(1),
            "non-local link must not be waited on, got {:?}",
            started.elapsed()
        );
        // And it is reported as unconnected by neither path (silent, like C).
        assert!(db.unconnected_external_links().await.is_empty());
    }

    // epics-base PR #336 — alias parsing + lookup integration tests.

    #[tokio::test]
    async fn alias_resolves_through_find_entry() {
        let db = PvDatabase::new();
        db.add_record(
            "TARGET",
            Box::new(crate::server::records::ai::AiRecord::new(42.0)),
        )
        .await
        .unwrap();
        db.add_alias("ALIAS_NAME", "TARGET").await.unwrap();

        // find_entry on the alias must return the same record as
        // find_entry on the target.
        let via_alias = db.find_entry("ALIAS_NAME").await;
        let via_target = db.find_entry("TARGET").await;
        assert!(via_alias.is_some());
        assert!(via_target.is_some());
        // has_name flips true for the alias too.
        assert!(db.has_name("ALIAS_NAME").await);
        assert!(db.has_name("TARGET").await);
        assert!(!db.has_name("NOT:THERE").await);
    }

    #[tokio::test]
    async fn alias_target_must_exist() {
        let db = PvDatabase::new();
        let err = db.add_alias("DANGLING", "MISSING_TARGET").await;
        assert!(err.is_err(), "alias to missing target must be rejected");
    }

    #[tokio::test]
    async fn alias_collision_with_existing_record_rejected() {
        let db = PvDatabase::new();
        db.add_record(
            "EXISTING",
            Box::new(crate::server::records::ai::AiRecord::new(0.0)),
        )
        .await
        .unwrap();
        db.add_record(
            "OTHER",
            Box::new(crate::server::records::ai::AiRecord::new(0.0)),
        )
        .await
        .unwrap();
        let err = db.add_alias("EXISTING", "OTHER").await;
        assert!(
            err.is_err(),
            "alias name colliding with record must be rejected"
        );
    }

    #[tokio::test]
    async fn get_record_resolves_alias() {
        // Regression: get_record must transparently resolve
        // aliases so dbpf / dbgf / dbpr / CA put paths see the same
        // record whether the caller uses the canonical name or the
        // alias.
        let db = PvDatabase::new();
        db.add_record(
            "TARGET",
            Box::new(crate::server::records::ai::AiRecord::new(0.0)),
        )
        .await
        .unwrap();
        db.add_alias("ALIAS", "TARGET").await.unwrap();

        let via_canonical = db.get_record("TARGET").await;
        let via_alias = db.get_record("ALIAS").await;
        assert!(via_canonical.is_some());
        assert!(via_alias.is_some(), "get_record must resolve alias");
        // Both calls return the same Arc (pointer equality).
        assert!(Arc::ptr_eq(&via_canonical.unwrap(), &via_alias.unwrap()));
    }

    #[tokio::test]
    async fn get_record_no_resolve_skips_alias_table() {
        // Strict variant must NOT see aliases — keeps the canonical
        // distinction available for builder code paths.
        let db = PvDatabase::new();
        db.add_record(
            "TARGET",
            Box::new(crate::server::records::ai::AiRecord::new(0.0)),
        )
        .await
        .unwrap();
        db.add_alias("ALIAS", "TARGET").await.unwrap();

        assert!(db.get_record_no_resolve("TARGET").await.is_some());
        assert!(
            db.get_record_no_resolve("ALIAS").await.is_none(),
            "get_record_no_resolve must not follow alias table"
        );
    }

    #[tokio::test]
    async fn register_cp_link_normalises_alias_to_canonical() {
        // Regression: CP link registration must store the
        // canonical record names. dispatch_cp_targets looks up by
        // canonical, so an alias-keyed entry is functionally dead.
        let db = PvDatabase::new();
        db.add_record(
            "SRC_REAL",
            Box::new(crate::server::records::ai::AiRecord::new(0.0)),
        )
        .await
        .unwrap();
        db.add_record(
            "DST_REAL",
            Box::new(crate::server::records::ai::AiRecord::new(0.0)),
        )
        .await
        .unwrap();
        db.add_alias("SRC_ALIAS", "SRC_REAL").await.unwrap();
        db.add_alias("DST_ALIAS", "DST_REAL").await.unwrap();

        // Register using the alias forms (CP edge: passive_only = false).
        db.register_cp_link("SRC_ALIAS", "DST_ALIAS", false).await;

        // Lookup must succeed via the canonical source name.
        let targets = db.get_cp_targets("SRC_REAL").await;
        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0].record, "DST_REAL");
        assert!(!targets[0].passive_only);
        // Alias-keyed lookup must NOT have been registered.
        let alias_lookup = db.get_cp_targets("SRC_ALIAS").await;
        assert!(alias_lookup.is_empty());
    }

    #[tokio::test]
    async fn aliases_for_record_returns_sorted_targets_only() {
        let db = PvDatabase::new();
        db.add_record(
            "TARGET",
            Box::new(crate::server::records::ai::AiRecord::new(0.0)),
        )
        .await
        .unwrap();
        db.add_record(
            "OTHER",
            Box::new(crate::server::records::ai::AiRecord::new(0.0)),
        )
        .await
        .unwrap();
        db.add_alias("ZZ", "TARGET").await.unwrap();
        db.add_alias("AA", "TARGET").await.unwrap();
        db.add_alias("MM", "OTHER").await.unwrap();

        // Sorted, only TARGET's aliases.
        assert_eq!(
            db.aliases_for_record("TARGET").await,
            vec!["AA".to_string(), "ZZ".to_string()]
        );
        // OTHER's alone.
        assert_eq!(db.aliases_for_record("OTHER").await, vec!["MM".to_string()]);
        // Unknown record → empty, not None.
        assert!(db.aliases_for_record("MISSING").await.is_empty());
    }

    #[tokio::test]
    async fn all_alias_names_returns_registered_aliases() {
        let db = PvDatabase::new();
        db.add_record(
            "TARGET",
            Box::new(crate::server::records::ai::AiRecord::new(0.0)),
        )
        .await
        .unwrap();
        db.add_alias("ALIAS_A", "TARGET").await.unwrap();
        db.add_alias("ALIAS_B", "TARGET").await.unwrap();

        let mut aliases = db.all_alias_names().await;
        aliases.sort();
        assert_eq!(aliases, vec!["ALIAS_A".to_string(), "ALIAS_B".to_string()]);
        // Canonical names are NOT returned here.
        assert!(!aliases.contains(&"TARGET".to_string()));
    }

    #[tokio::test]
    async fn complete_async_record_accepts_alias() {
        // Invariant audit: complete_async_record (the
        // entry point used by async device-support callbacks to
        // finish processing) must accept an alias name. Pre-fix it
        // walked `inner.records` directly and would
        // `ChannelNotFound` if the original name was an alias.
        let db = PvDatabase::new();
        db.add_record(
            "TARGET",
            Box::new(crate::server::records::ai::AiRecord::new(0.0)),
        )
        .await
        .unwrap();
        db.add_alias("ALIAS", "TARGET").await.unwrap();

        // Use complete_async_record by alias — must not error.
        db.complete_async_record("ALIAS").await.unwrap();
        // And by canonical too — keeps existing behaviour.
        db.complete_async_record("TARGET").await.unwrap();
    }

    #[tokio::test]
    async fn process_record_accepts_alias() {
        // Regression: process_record must accept an alias
        // name. Pre-fix it walked `inner.records` directly.
        let db = PvDatabase::new();
        db.add_record(
            "TARGET",
            Box::new(crate::server::records::ai::AiRecord::new(0.0)),
        )
        .await
        .unwrap();
        db.add_alias("ALIAS", "TARGET").await.unwrap();

        // Both should succeed and reach the same record.
        db.process_record("TARGET").await.unwrap();
        db.process_record("ALIAS").await.unwrap();

        // A bogus name still errors.
        assert!(db.process_record("MISSING").await.is_err());
    }

    #[tokio::test]
    async fn process_record_with_links_accepts_alias_and_avoids_cycle() {
        // Regression: process_record_with_links normalises
        // the alias so that (a) the records-map lookup hits and
        // (b) the cycle-detection set doesn't treat alias and
        // canonical as two distinct entries (which would let a
        // self-loop slip past the visited check).
        let db = PvDatabase::new();
        db.add_record(
            "TARGET",
            Box::new(crate::server::records::ai::AiRecord::new(0.0)),
        )
        .await
        .unwrap();
        db.add_alias("ALIAS", "TARGET").await.unwrap();

        let mut visited = std::collections::HashSet::new();
        db.process_record_with_links("ALIAS", &mut visited, 0)
            .await
            .unwrap();

        // visited should contain the *canonical* name only.
        assert!(
            visited.contains("TARGET"),
            "visited must record the canonical name: {visited:?}",
        );
        assert!(
            !visited.contains("ALIAS"),
            "visited must NOT record the alias form: {visited:?}",
        );
    }

    #[tokio::test]
    async fn alias_duplicate_rejected() {
        let db = PvDatabase::new();
        db.add_record(
            "TARGET",
            Box::new(crate::server::records::ai::AiRecord::new(0.0)),
        )
        .await
        .unwrap();
        db.add_alias("ALIAS", "TARGET").await.unwrap();
        // Re-registering the same alias name (even to the same target)
        // must fail — base behaviour: aliases are inserted once.
        let err = db.add_alias("ALIAS", "TARGET").await;
        assert!(err.is_err(), "duplicate alias name must be rejected");
    }

    /// `add_pv`, `add_pv_with_hook`, and `add_record` must
    /// refuse to silently replace an existing registration. Mirrors
    /// epics-base C IOC which treats a duplicate `dbLoadRecords` name
    /// as a fatal load error.
    #[tokio::test]
    async fn add_pv_and_add_record_reject_duplicates_across_namespaces() {
        use crate::server::records::ai::AiRecord;

        let db = PvDatabase::new();
        db.add_pv("A", EpicsValue::Double(1.0)).await.unwrap();
        // Same name as simple_pv — every namespace must see it.
        assert!(db.add_pv("A", EpicsValue::Double(2.0)).await.is_err());
        let noop_hook: crate::server::pv::WriteHook =
            std::sync::Arc::new(|_v, _ctx| Box::pin(async { Ok(()) }));
        assert!(
            db.add_pv_with_hook("A", EpicsValue::Double(2.0), noop_hook)
                .await
                .is_err()
        );
        assert!(
            db.add_record("A", Box::new(AiRecord::new(0.0)))
                .await
                .is_err()
        );
        assert!(db.add_alias("A", "A").await.is_err());

        db.add_record("R", Box::new(AiRecord::new(0.0)))
            .await
            .unwrap();
        assert!(
            db.add_record("R", Box::new(AiRecord::new(1.0)))
                .await
                .is_err()
        );
        assert!(db.add_pv("R", EpicsValue::Double(0.0)).await.is_err());
        assert!(db.add_alias("R", "R").await.is_err());

        db.add_alias("AL", "R").await.unwrap();
        assert!(db.add_pv("AL", EpicsValue::Double(0.0)).await.is_err());
        assert!(
            db.add_record("AL", Box::new(AiRecord::new(0.0)))
                .await
                .is_err()
        );
    }

    /// Removing a record must purge aliases
    /// that pointed AT it. Otherwise the alias name stays
    /// "registered" forever and `add_pv` / `add_record` rejecting
    /// reuse causes a permanent name leak.
    #[tokio::test]
    async fn remove_record_purges_dangling_aliases() {
        use crate::server::records::ai::AiRecord;

        let db = PvDatabase::new();
        db.add_record("R", Box::new(AiRecord::new(0.0)))
            .await
            .unwrap();
        db.add_alias("ALT1", "R").await.unwrap();
        db.add_alias("ALT2", "R").await.unwrap();
        // An alias that points elsewhere must NOT be touched.
        db.add_record("OTHER", Box::new(AiRecord::new(0.0)))
            .await
            .unwrap();
        db.add_alias("KEEPER", "OTHER").await.unwrap();

        assert!(db.remove_record("R").await);

        // Both aliases pointing at R should be gone — `add_pv` of
        // those names succeeds again.
        db.add_pv("ALT1", EpicsValue::Double(0.0)).await.unwrap();
        db.add_pv("ALT2", EpicsValue::Double(0.0)).await.unwrap();
        // The unrelated alias must survive.
        assert_eq!(db.resolve_alias("KEEPER").await, Some("OTHER".to_string()));
    }

    /// `add_alias` must reject collisions with
    /// every namespace, including simple PVs (which the pre-fix
    /// code missed).
    #[tokio::test]
    async fn add_alias_rejects_simple_pv_collision() {
        use crate::server::records::ai::AiRecord;

        let db = PvDatabase::new();
        db.add_pv("PVX", EpicsValue::Double(0.0)).await.unwrap();
        db.add_record("TARGET", Box::new(AiRecord::new(0.0)))
            .await
            .unwrap();
        // alias name "PVX" collides with the simple PV — must fail.
        assert!(db.add_alias("PVX", "TARGET").await.is_err());
    }

    /// Concurrent `add_pv` and `add_record` with
    /// the same name must not deadlock and must serialize so that
    /// exactly one succeeds. Pre-fix the two methods grabbed
    /// different write locks first, opening a cross-lock-order
    /// deadlock window.
    #[tokio::test]
    async fn concurrent_add_pv_and_add_record_do_not_deadlock() {
        use crate::server::records::ai::AiRecord;

        let db = std::sync::Arc::new(PvDatabase::new());
        let db1 = db.clone();
        let db2 = db.clone();
        let h1 = tokio::spawn(async move { db1.add_pv("RACE", EpicsValue::Double(1.0)).await });
        let h2 =
            tokio::spawn(async move { db2.add_record("RACE", Box::new(AiRecord::new(0.0))).await });
        // Both complete within a reasonable bound — pre-fix this
        // could hang because T1 holds simple_pvs.write and waits
        // for records.read while T2 holds records.write and waits
        // for simple_pvs.read.
        let r1 = tokio::time::timeout(std::time::Duration::from_secs(2), h1)
            .await
            .expect("add_pv must not block on add_record");
        let r2 = tokio::time::timeout(std::time::Duration::from_secs(2), h2)
            .await
            .expect("add_record must not block on add_pv");
        let r1 = r1.unwrap();
        let r2 = r2.unwrap();
        // Exactly one of the two wins; the other reports
        // "already registered".
        assert!(
            (r1.is_ok() && r2.is_err()) || (r1.is_err() && r2.is_ok()),
            "exactly one of the racing inserts must succeed: r1={r1:?} r2={r2:?}",
        );
    }

    #[tokio::test]
    async fn existence_gate_blocks_cached_simple_pv_per_request() {
        // A cached simple PV must re-pass the installed existence gate on
        // both the search (`has_name_from`) and create (`find_entry_from`)
        // paths. Records bypass the gate. With no gate the short-circuit
        // is unchanged (plain-IOC behaviour).
        use std::net::SocketAddr;

        let db = PvDatabase::new();
        db.add_pv("SHADOW:x", EpicsValue::Double(1.0))
            .await
            .unwrap();
        db.add_record(
            "REC",
            Box::new(crate::server::records::ai::AiRecord::new(0.0)),
        )
        .await
        .unwrap();

        let denied: SocketAddr = "127.0.0.1:5064".parse().unwrap();
        let allowed: SocketAddr = "192.0.2.5:5064".parse().unwrap();

        // No gate installed: the cached simple PV resolves unconditionally.
        assert!(db.has_name_from("SHADOW:x", Some(denied)).await);
        assert!(db.find_entry_from("SHADOW:x", Some(denied)).await.is_some());

        // Gate denies the simple PV only for `denied` (the gateway's
        // host-scoped `.pvlist` admission has exactly this shape).
        let gate: ExistenceGate = Arc::new(move |name, peer| {
            Box::pin(async move { !(name == "SHADOW:x" && peer == Some(denied)) })
        });
        db.set_existence_gate(gate).await;

        // Denied peer: does-not-exist on both paths despite the PV being
        // cached in `simple_pvs`.
        assert!(!db.has_name_from("SHADOW:x", Some(denied)).await);
        assert!(db.find_entry_from("SHADOW:x", Some(denied)).await.is_none());

        // Allowed peer: still resolves.
        assert!(db.has_name_from("SHADOW:x", Some(allowed)).await);
        assert!(
            db.find_entry_from("SHADOW:x", Some(allowed))
                .await
                .is_some()
        );

        // Records are never gateway-managed — the gate must not gate them
        // even for the denied peer.
        assert!(db.has_name_from("REC", Some(denied)).await);
        assert!(db.find_entry_from("REC", Some(denied)).await.is_some());
    }

    /// `record_link_fields` must surface a record's device-support `INP`
    /// link. An `ai`'s `INP` is a `DBF_INLINK` field stored in
    /// `common.inp` — it is not a `DbFieldType::String` entry in
    /// `field_list()` — so the earlier `field_list()` scan filtered by
    /// `String` silently dropped it. The pvalink install scan walks this
    /// method, so a Passive `ai` carrying a CP/CPP pvalink `INP` never had
    /// its monitor opened at iocInit. Enumerating the canonical
    /// `common.inp` storage fixes it; C `dbpvar`/`dbcar` likewise dump
    /// every link field including device-support INP/OUT.
    #[tokio::test]
    async fn record_link_fields_surfaces_device_support_inp() {
        use crate::server::record::ParsedLink;
        use crate::server::records::ai::AiRecord;

        let db = PvDatabase::new();
        db.add_record("AI", Box::new(AiRecord::new(0.0)))
            .await
            .unwrap();
        // Device-support INP lives in `common.inp` (DBF_INLINK), the
        // exact storage a `field_list()` String scan cannot reach.
        {
            let rec = db.get_record("AI").await.unwrap();
            rec.write().await.common.inp = "pva://mini:current?proc=CP".to_string();
        }

        let links = db.record_link_fields("AI").await;
        let inp = links
            .iter()
            .find(|(f, _, _)| f == "INP")
            .unwrap_or_else(|| panic!("INP link must be surfaced, got {links:?}"));
        assert_eq!(inp.1, "pva://mini:current?proc=CP");
        assert!(
            matches!(inp.2, ParsedLink::Pva(_)),
            "a pva:// INP must parse to ParsedLink::Pva, got {:?}",
            inp.2
        );
    }
}
