//! [`CaLinkResolver`] — a [`LinkSet`] backend that resolves CA record
//! links through a live [`crate::client::CaClient`].
//!
//! Each distinct CA-link PV name gets one [`CaLink`]: a CA channel, a
//! subscription, and a monitor task that keeps an [`arc_swap::ArcSwap`]
//! snapshot current. The [`LinkSet`] read methods serve from that
//! cache — never a synchronous per-read network fetch. This is the
//! C `dbCa.c` model: `dbCaGetLink` (`dbCa.c:448`) reads the value
//! cached by the monitor `eventCallback` (`dbCa.c:925`).

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::DbFieldType;
use crate::client::{CaChannel, CaClient};
use crate::protocol::{DBE_ALARM, DBE_VALUE};
use arc_swap::ArcSwap;
use epics_base_rs::runtime::task;
use epics_base_rs::server::database::{
    LinkDbfType, LinkMetadata, LinkPutOp, LinkSet, PutAdmission, PvDatabase,
};
use epics_base_rs::server::snapshot::{DbrClass, Snapshot};
use epics_base_rs::types::DBR_CTRL_DOUBLE;
use epics_base_rs::types::EpicsValue;
use parking_lot::RwLock;

/// CA record-link monitor event mask — `DBE_VALUE | DBE_ALARM`, matching
/// C `dbCa`'s `ca_add_array_event` (`dbCa.c:1258-1269`), whose libca macro
/// expands to `ca_add_masked_array_event(..., DBE_VALUE | DBE_ALARM)`
/// (`cadef.h:2004-2012`). Deliberately excludes `DBE_LOG` / `DBE_ARCHIVE`
/// (a separate event-trigger class, `cadef.h:1148-1158`) that `dbCa` never
/// requests, so archive/log-only posts on the upstream PV never refresh a
/// CP/CPP record link's cache or wake a scan. The default
/// `CaChannel::subscribe()` would add `DBE_LOG`.
const CALINK_EVENT_MASK: u16 = DBE_VALUE | DBE_ALARM;

/// A cached monitor snapshot plus the channel native element count it was
/// produced under.
///
/// The native DBR *type* is intrinsic to `snapshot.value` (recomputed at
/// read time via [`EpicsValue::dbr_type`]), but the native element *count*
/// is not recoverable from a possibly-partial waveform payload, so it is
/// captured here at store time. [`CaLink`]'s read accessors serve this only
/// while both still match the channel's current native description — see
/// [`CaLink::with_servable`] / C `dbCa.c:865-889`.
struct CachedSnapshot {
    snapshot: Snapshot,
    native_count: u32,
}

/// True iff a cached snapshot remains servable: the value's CA-wire DBR
/// type and the channel native element count it was taken under both still
/// equal the channel's current native description. Mirrors C
/// `dbCa.c:865-889`, which refuses the old cache once a reconnect changes
/// the element count or DBR type, until a matching monitor event
/// repopulates it (`dbCaGetLink` invalid-cache path `dbCa.c:484-492`).
///
/// Pure (no `self`) so the type/count gate is unit-testable without a live
/// CA channel — the same factoring as [`note_conn_event`].
fn cache_native_matches(
    cached_type: DbFieldType,
    cached_count: u32,
    current_type: DbFieldType,
    current_count: u32,
) -> bool {
    cached_type == current_type && cached_count == current_count
}

/// Errors from the CA-link resolver setup path.
#[derive(Debug, thiserror::Error)]
pub enum CaLinkError {
    /// The shared [`CaClient`] could not be constructed.
    #[error("CA client init failed: {0}")]
    ClientInit(String),
    /// Subscribing the monitor for a CA link failed.
    #[error("CA link subscribe failed for {pv}: {reason}")]
    Subscribe { pv: String, reason: String },
}

/// One open CA link — a monitor-backed cache of a remote PV.
///
/// Mirrors C `caLink` (`dbCa.c`): a CA channel plus a subscription
/// whose callback refreshes the cached value. The cache is the only
/// thing the synchronous [`LinkSet`] read path touches. An opaque
/// handle — construct it via [`CaLinkResolver::open`].
pub struct CaLink {
    /// Latest monitor snapshot plus the native description it was produced
    /// under. `None` until the first event arrives (channel not yet
    /// connected / no value cached) — the C `dbCaGetLink` "not connected"
    /// case. Served only while the cached description still matches the
    /// channel's current native description (see [`Self::with_servable`]).
    cache: Arc<ArcSwap<Option<CachedSnapshot>>>,
    /// Live-connection flag, mirroring `pvalink`'s
    /// `PvaLink::monitor_connected`. The connection-event watcher task
    /// flips this `true` on `ConnectionEvent::Connected` and `false`
    /// on `Disconnected` / `Unresponsive`. `is_connected()` reads it
    /// so a downstream IOC restart is reflected as a real disconnect
    /// — pre-fix `is_connected()` keyed off cache presence alone and
    /// stayed `true` forever once any event had been cached, serving
    /// the last stale `Snapshot` through the whole outage with no
    /// LINK alarm. `dbCa.c` sets `pca->connected = FALSE` in its
    /// `connectionCallback` for exactly this reason.
    ///
    /// Owned by [`LinkConnState`], not a bare flag: C's
    /// `connectionCallback` does two things on a disconnect — clears the
    /// flag AND adds `CA_DBPROCESS` for the link's CP holders
    /// (`dbCa.c:862-873`) — and a bare `AtomicBool` let three call sites
    /// do the first without the second (stage C6 criterion 4).
    connected: Arc<LinkConnState>,
    /// Cached remote CTRL attributes (display/control/alarm limits,
    /// precision, units) plus the channel's native DBF type and element
    /// count. `None` until the first attribute fetch completes; the
    /// connection-event watcher re-fetches on every (re)connection.
    /// Mirrors C `dbCa.c`: `connectionCallback`
    /// (`dbCa.c:833`) schedules `CA_GET_ATTRIBUTES` on connect, and
    /// `getAttribEventCallback` (`dbCa.c:1080`) caches the
    /// `DBR_CTRL_DOUBLE` reply that `getControlLimits`/`getGraphicLimits`
    /// /`getAlarmLimits`/`getPrecision`/`getUnits` (`dbCa.c:726`) later
    /// serve.
    meta: Arc<ArcSwap<Option<LinkMetadata>>>,
    /// The CA channel — kept alive so the monitor stays subscribed.
    /// Used by the OUT-link write path.
    channel: Arc<CaChannel>,
    /// Abort-on-drop handle for the monitor task. Dropping the
    /// `CaLink` stops the task and (via `MonitorHandle::drop`)
    /// unsubscribes the remote monitor.
    _monitor_task: AbortOnDrop,
    /// Abort-on-drop handle for the connection-event watcher task.
    /// Drains `CaChannel::connection_events()` and keeps `connected`
    /// in sync with the real circuit state.
    _conn_task: AbortOnDrop,
}

/// The single owner of a `ca://` link's connection-state transition.
///
/// **Invariant.** A `ca://` link's servability flag MUST NOT go from
/// connected to disconnected without dispatching that PV's CP/CPP holders.
/// C `connectionCallback` (`dbCa.c:861-873`) clears `pca->isConnected` and,
/// in the same critical section, sets `CA_DBPROCESS` for a `pvlOptCP` link
/// (or a `pvlOptCPP` link whose holder is Passive), so the holder processes,
/// its `dbCaGetLink` returns `-1` with `LINK_ALARM`/`INVALID_ALARM`
/// (`dbCa.c:459-463`), and the record lands in LINK/INVALID. Without the
/// dispatch a Passive CP holder is never processed again and keeps serving
/// its last good value with `SEVR=NO_ALARM` for the whole outage — measured
/// on target, stage C6 criterion 4 (§11.4).
///
/// **Owner.** This type. The flag is private, so the three sites that used
/// to `store` into it directly (the connection watcher's two arms and
/// `run_monitor`'s subscription-ended tail) now have to go through
/// [`Self::mark_connected`] / [`Self::mark_disconnected`], and the dispatch
/// cannot be forgotten at a new fourth site.
///
/// Only the true→false EDGE dispatches: `Disconnected` can arrive repeatedly
/// (the watcher sees it, then the subscription ends), and C reaches
/// `CA_DBPROCESS` once per `connectionCallback` with a real state change.
/// The `swap` makes that an atomic test-and-set rather than a load-then-store
/// race between the watcher task and the monitor task.
///
/// Reconnect does NOT dispatch here, matching C: `connectionCallback`'s
/// connect arm schedules attribute and monitor work, and it is the monitor
/// event that drives processing — which [`run_monitor`] already does.
///
/// The same owner also carries the read half of C's cached access rights
/// (`pca->hasReadAccess`, `dbCa.c:875`/`:1089`). [`Self::note_access_rights`]
/// is its only writer, and it performs C `accessRightsCallback`'s two steps
/// (`dbCa.c:1076-1102`) — cache the new rights, then dispatch the CP/CPP
/// holders when a right is lost while connected — as one owned transition,
/// for the same reason the disconnect dispatch lives inside
/// [`Self::mark_disconnected`]: a second site that stored the rights without
/// dispatching would be the §11.4 defect again on the access axis.
struct LinkConnState {
    flag: AtomicBool,
    /// C `pca->hasReadAccess` (`dbCa.c:875`, `:1089`) — the read half of the
    /// last server-granted access rights. Consulted ONLY by the value read
    /// ([`CaLink::value`]), exactly as C consults it only in `dbCaGetLink`
    /// (`dbCa.c:459`); the severity/timestamp/metadata getters (`pcaGetCheck`,
    /// `dbCa.c:650-660`) and the lset `isConnected` (`dbCa.c:633-641`) check
    /// `isConnected` alone.
    ///
    /// `true` at rest, NOT C's calloc-FALSE. C sets `isConnected` and both
    /// access flags inside one `pca->lock` critical section
    /// (`dbCa.c:861-876`), so a connected link never shows the calloc
    /// default. Here `Connected` and the rights that follow it are two
    /// broadcast events, and [`run_monitor`] may open the connected gate
    /// first (a delivered event is proof of liveness), so a `false` default
    /// would refuse the first monitor value of every ordinary full-rights
    /// link until the watcher caught up — a spurious startup LINK alarm C
    /// never shows. The coordinator broadcasts the real rights immediately
    /// after every `Connected`, so the default only lives for that gap.
    read_access: AtomicBool,
    /// C `pca->hasWriteAccess` (`dbCa.c:876`, `:1090`) — the write half of
    /// the same rights event, consulted by [`CaResolver::put_admission`]
    /// because `dbCaPutLinkCallback`'s gate is
    /// `if (!pca->isConnected || !pca->hasWriteAccess)` (`dbCa.c:558-561`):
    /// BOTH operands, tested before anything is staged. The client's own
    /// put path also refuses a write-denied channel (libca `nciu::write`
    /// ECA_NOWTACCESS parity), but that refusal happens on the link work
    /// owner, long after the record cycle that issued the write has
    /// finished — so it lands no LINK/INVALID on the owning record, and the
    /// put-notify flavour completes its wait-set as a success. The gate has
    /// to be whole HERE, where C puts it.
    ///
    /// `true` at rest for the same reason as `read_access` above.
    write_access: AtomicBool,
    /// The database whose CP/CPP holders of `pv_name` must be processed on a
    /// disconnect. Attached after the resolver is mounted, hence the lock and
    /// the `Option`; a `None` here is a link opened with no database, which
    /// has no holders to process.
    db: Arc<RwLock<Option<PvDatabase>>>,
    pv_name: String,
}

impl LinkConnState {
    fn new(db: Arc<RwLock<Option<PvDatabase>>>, pv_name: String) -> Self {
        Self {
            flag: AtomicBool::new(false),
            read_access: AtomicBool::new(true),
            write_access: AtomicBool::new(true),
            db,
            pv_name,
        }
    }

    fn is_set(&self) -> bool {
        self.flag.load(Ordering::Acquire)
    }

    /// Mark the link live. Returns `true` on the false→true edge.
    fn mark_connected(&self) -> bool {
        !self.flag.swap(true, Ordering::AcqRel)
    }

    /// Mark the link dead and, on the true→false edge, process every local
    /// CP/CPP holder of this PV so the holder's failed link read commits
    /// LINK/INVALID. Returns `true` on the edge.
    fn mark_disconnected(&self) -> bool {
        if !self.flag.swap(false, Ordering::AcqRel) {
            return false;
        }
        // Drop the read guard before dispatching: the dispatch runs record
        // processing, and holding a lock across it is the deadlock shape
        // `run_monitor` already avoids on the value path.
        let db_handle = self.db.read().clone();
        if let Some(db_handle) = db_handle {
            db_handle.dispatch_external_cp_targets(&self.pv_name);
        }
        true
    }

    /// The read half of the cached access rights — C `dbCaGetLink`'s
    /// `!pca->hasReadAccess` operand (`dbCa.c:459`).
    fn has_read_access(&self) -> bool {
        self.read_access.load(Ordering::Acquire)
    }

    /// The write half — `dbCaPutLinkCallback`'s `!pca->hasWriteAccess`
    /// operand (`dbCa.c:558`).
    fn has_write_access(&self) -> bool {
        self.write_access.load(Ordering::Acquire)
    }

    /// C `accessRightsCallback` (`dbCa.c:1076-1102`) as one owned
    /// transition. Returns `true` iff the CP/CPP holders were dispatched.
    ///
    /// * **Not connected:** do nothing at all — C returns before touching
    ///   the cached flags (`dbCa.c:1084-1085`, "connectionCallback will
    ///   handle"). Safe here for the same reason it is in C: the
    ///   coordinator re-broadcasts the current rights immediately after
    ///   `Connected` on every (re)connect, so a skipped event is always
    ///   superseded, and skipping is what keeps a stale rights event
    ///   queued behind a `Disconnected` from double-dispatching an outage
    ///   the disconnect edge already dispatched.
    /// * **Connected:** cache both new rights, then dispatch the
    ///   holders UNLESS both read and write are held (`dbCa.c:1091`
    ///   `if (hasReadAccess && hasWriteAccess) goto done`). C processes on
    ///   the loss of EITHER right — not read loss alone — and processes
    ///   NOTHING on a full regain: the holder's alarm clears on the next
    ///   monitor event, not here. The dispatch is per rights change (one
    ///   `accessRightsCallback` per change in C), not edge-deduplicated.
    ///
    /// A dispatched holder whose link lost READ access fails its value
    /// read ([`CaLink::value`] gates on [`Self::has_read_access`]) and
    /// commits LINK/INVALID — `dbCa.c:459-463`. A holder whose link lost
    /// only WRITE access still reads a good value and lands no alarm,
    /// which is C's outcome too (`dbCaGetLink` does not consult
    /// `hasWriteAccess`).
    fn note_access_rights(&self, read: bool, write: bool) -> bool {
        if !self.flag.load(Ordering::Acquire) {
            return false;
        }
        self.read_access.store(read, Ordering::Release);
        self.write_access.store(write, Ordering::Release);
        if read && write {
            return false;
        }
        // Drop the read guard before dispatching — same rule as
        // `mark_disconnected`: the dispatch runs record processing, and
        // holding a lock across it is the deadlock shape `run_monitor`
        // avoids on the value path.
        let db_handle = self.db.read().clone();
        if let Some(db_handle) = db_handle {
            db_handle.dispatch_external_cp_targets(&self.pv_name);
        }
        true
    }
}

/// Abort the wrapped task when dropped. A bare handle detaches on drop
/// and would leak the monitor task. Typed on the `runtime::task` spawn
/// seam ([`task::TaskHandle`]) so the monitor/watcher tasks route through
/// the same executor on both backends — `tokio::spawn` on the host, the
/// callback-band future executor on RTEMS — rather than a bare
/// `tokio::task::JoinHandle` that pins calink to the tokio runtime.
struct AbortOnDrop(task::TaskHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

impl CaLink {
    /// Run `f` over the currently-servable cached snapshot, or return
    /// `None`. The single gate every value-derived accessor shares — the C
    /// `dbCaGetLink` readable-cache check (`dbCa.c:448`, `:484-492`):
    ///
    /// 1. the circuit is up (`connected`, driven by
    ///    `CaChannel::connection_events()`);
    /// 2. a monitor snapshot is cached (the C `pca->pgetNative` populated
    ///    case); AND
    /// 3. that snapshot's native description (DBR type + element count)
    ///    still matches the channel's CURRENT native description.
    ///
    /// (3) is the BRIDGE-106 fix: after an upstream reconnect changes the
    /// type or element count (C `dbCa.c:865-889`), the snapshot cached
    /// under the old description stops being servable until a new monitor
    /// event repopulates a matching cache. The check is read-side and
    /// value-intrinsic, so it has no dependence on the ordering between the
    /// connection-event watcher and the monitor task.
    fn with_servable<R>(&self, f: impl FnOnce(&Snapshot) -> R) -> Option<R> {
        if !self.connected.is_set() {
            return None;
        }
        let guard = self.cache.load();
        let cached = guard.as_ref().as_ref()?;
        if !self.cache_matches_channel(cached) {
            return None;
        }
        Some(f(&cached.snapshot))
    }

    /// Whether `cached`'s native description still matches the channel's
    /// current one. A disconnected channel (no current description) is not
    /// servable. Thin wrapper over the pure [`cache_native_matches`].
    fn cache_matches_channel(&self, cached: &CachedSnapshot) -> bool {
        match (
            self.channel.native_field_type(),
            self.channel.element_count(),
        ) {
            (Ok(cur_type), Ok(cur_count)) => cache_native_matches(
                cached.snapshot.value.dbr_type(),
                cached.native_count,
                cur_type,
                cur_count,
            ),
            _ => false,
        }
    }

    /// True when the CA circuit is currently up AND a monitor event whose
    /// native description still matches the channel has been cached. C
    /// `dbCaGetLink` (`dbCa.c:448`) treats a CA link as readable only when
    /// `pca->connected` is set (the `connectionCallback` clears it on
    /// disconnect) *and* the monitor callback has populated a matching
    /// `pca->pgetNative` (cleared by `dbCa.c:865-889` on a type/count
    /// change).
    ///
    /// Pre-fix this keyed off cache presence alone, so an upstream IOC
    /// restart was invisible — `is_connected()` stayed `true` and stale
    /// data was served with no LINK alarm; a later refinement added the
    /// circuit-state flag, and BRIDGE-106 added the type/count match.
    pub fn is_connected(&self) -> bool {
        self.with_servable(|_| ()).is_some()
    }

    /// The cached write right — `dbCaPutLinkCallback`'s second operand
    /// (`dbCa.c:558`). Separate from [`Self::is_connected`] because C tests
    /// them separately and a write-denied link is still connected: it keeps
    /// serving values to `dbCaGetLink`, which does not consult this.
    pub fn has_write_access(&self) -> bool {
        self.connected.has_write_access()
    }

    /// The iocInit wait's all-conditions gate for this link — C
    /// `testInitReady` (dbCa.c:835-845, epics-base #856 "dbCa: iocInit
    /// wait for all conditions"): servable (connected with the first
    /// monitor event cached — C's NATIVE wait bit) AND the attribute
    /// fetch complete (the ATTRIB bit; `fetch_link_metadata` stores
    /// `Some` even when the CTRL get failed, exactly the
    /// action-completed edge `getAttribEventCallback` clears the bit
    /// on). C's STRING bit has no twin here: this port keeps one native
    /// monitor and renders strings from it.
    pub fn init_ready(&self) -> bool {
        self.is_connected() && self.meta.load().is_some()
    }

    /// Current cached value, or `None` when the link is not servable: no
    /// event yet, the circuit is down, READ access is denied, or the cached
    /// snapshot's type/count no longer matches the channel after an
    /// upstream type/count change (C `dbCaGetLink` "not connected" /
    /// invalid-cache paths). A non-servable link serves no value, so a
    /// downstream IOC outage or type change does not leak a stale/
    /// mis-shaped value into the owning record.
    pub fn value(&self) -> Option<EpicsValue> {
        // C `dbCaGetLink`'s full gate is `!pca->isConnected ||
        // !pca->hasReadAccess` (`dbCa.c:459-463`). The read-access half
        // lives HERE and not in `with_servable`, because C consults
        // `hasReadAccess` only for the value read — `pcaGetCheck`
        // (severity/timestamp/DBF getters, `dbCa.c:650-660`) and the lset
        // `isConnected` (`dbCa.c:633-641`) check `isConnected` alone. A
        // read-denied link therefore still reports connected (its circuit
        // is up; writes may proceed) but serves no value, so a dispatched
        // CP holder's link read fails and commits LINK/INVALID exactly as
        // on the disconnect edge.
        if !self.connected.has_read_access() {
            return None;
        }
        self.with_servable(|s| s.value.clone())
    }

    /// Current cached alarm severity (0..3), or `None` when the link
    /// is not connected. Mirrors C `dbCaGetAlarmLimits` reading the
    /// cached `pca->sevr` — gated on `pca->connected`.
    pub fn alarm_severity(&self) -> Option<i32> {
        self.with_servable(|s| s.alarm.severity as i32)
    }

    /// Cached alarm *status* code (the EPICS `alarm_status` enum), or
    /// `None` when the link is not connected. lets an
    /// `MSS`-modified CA link propagate the remote STAT into the owning
    /// record instead of the generic `LINK_ALARM`. Gated on `connected`
    /// exactly like [`Self::alarm_severity`].
    pub fn alarm_status(&self) -> Option<i32> {
        self.with_servable(|s| s.alarm.status as i32)
    }

    /// Cached timestamp as `(seconds_past_epoch, nanoseconds, userTag)`,
    /// or `None` when the link is not connected. The Channel Access wire
    /// protocol's `DBR_TIME_*` payload carries no user tag, so the tag is
    /// always `0` — only PVA links can adopt a remote `timeStamp.userTag`.
    pub fn time_stamp(&self) -> Option<(i64, i32, u64)> {
        self.with_servable(|s| {
            let dur = s.timestamp.since_unix_epoch();
            Some((dur.as_secs() as i64, dur.subsec_nanos() as i32, 0))
        })
        .flatten()
    }

    /// Cached remote metadata (display/control/alarm limits, precision,
    /// units, DBF type, element count), or `None` when the link is not
    /// connected. Gated on `connected` exactly like
    /// [`Self::value`]/[`Self::alarm_severity`] — C `pcaGetCheck`
    /// (`dbCa.c:650`) returns `-1` from every metadata getter while the
    /// CA link is disconnected, so the owning record keeps its local
    /// default rather than adopting stale remote limits.
    pub fn link_metadata(&self) -> Option<LinkMetadata> {
        if !self.connected.is_set() {
            return None;
        }
        self.meta.load().as_ref().clone()
    }
}

/// A [`LinkSet`] backend for the `ca` URL scheme.
///
/// Holds a single shared [`CaClient`] and a registry of open
/// [`CaLink`]s keyed by PV name, so multiple records pointing at the
/// same remote PV share one CA channel + subscription. Cheap to
/// clone — every field is `Arc`-backed.
#[derive(Clone)]
pub struct CaLinkResolver {
    /// Shared CA client, created lazily on the first link [`Self::open`].
    /// An IOC with no CA links never spins one up — the same lazy-client
    /// shape as `pvalink`'s per-link `PvaClient` (the C `dbCa` client is
    /// likewise only created once a link is added). Seeded eagerly by
    /// [`Self::with_client`] when a caller wants to share/pin a client.
    client: Arc<tokio::sync::OnceCell<Arc<CaClient>>>,
    /// Open links keyed by bare PV name (`ca://` scheme stripped).
    links: Arc<RwLock<HashMap<String, Arc<CaLink>>>>,
    /// Database handle the monitor callback uses to process external
    /// CP/CPP holder records on each remote change
    /// ([`PvDatabase::dispatch_external_cp_targets`]). `None` until
    /// [`Self::attach_database`] is called at IOC assembly. Late-bound
    /// behind a lock so the cheaply-`Clone`d resolver can have the DB
    /// attached after construction — the same shape as `pvalink`'s
    /// `PvaLinkResolver::db`.
    db: Arc<RwLock<Option<PvDatabase>>>,
}

impl Default for CaLinkResolver {
    fn default() -> Self {
        Self::new()
    }
}

impl CaLinkResolver {
    /// Build a resolver whose shared [`CaClient`] is created lazily on
    /// the first link [`Self::open`]. Infallible — an IOC with no CA
    /// links never constructs a client, and any client-init failure
    /// surfaces at the first `open` (mirroring `pvalink`'s lazy client),
    /// so installation cannot fail and never aborts the IOC.
    pub fn new() -> Self {
        Self {
            client: Arc::new(tokio::sync::OnceCell::new()),
            links: Arc::new(RwLock::new(HashMap::new())),
            db: Arc::new(RwLock::new(None)),
        }
    }

    /// Build a resolver around an already-constructed [`CaClient`].
    /// Lets a caller share one client across the CA gateway and the
    /// CA links, or pin the client to a specific server in tests. The
    /// client is seeded eagerly, so the lazy-init path in [`Self::open`]
    /// is bypassed.
    pub fn with_client(client: Arc<CaClient>) -> Self {
        Self {
            client: Arc::new(tokio::sync::OnceCell::new_with(Some(client))),
            links: Arc::new(RwLock::new(HashMap::new())),
            db: Arc::new(RwLock::new(None)),
        }
    }

    /// Get the shared [`CaClient`], creating it on first call. The
    /// `OnceCell` guarantees exactly one client even under concurrent
    /// first opens. A client-init failure is returned (not cached), so a
    /// later open retries — matching C `dbCa`'s deferred channel setup.
    async fn client(&self) -> Result<&Arc<CaClient>, CaLinkError> {
        self.client
            .get_or_try_init(|| async {
                CaClient::new()
                    .await
                    .map(Arc::new)
                    .map_err(|e| CaLinkError::ClientInit(e.to_string()))
            })
            .await
    }

    /// Attach the database handle the monitor callback uses to process
    /// external CP/CPP holder records on each remote change
    /// ([`PvDatabase::dispatch_external_cp_targets`]). Called by
    /// [`install_calink_resolver`] at IOC assembly — before iocInit's
    /// `setup_cp_links` warms any external CP link — so the handle is
    /// always present by the time the first monitor event fires. The
    /// cross-IOC twin of `pvalink`'s `PvaLinkResolver::attach_database`.
    pub fn attach_database(&self, db: PvDatabase) {
        *self.db.write() = Some(db);
    }

    /// Open / cache the CA link for `pv_name` (a bare PV name, no
    /// `ca://` scheme). Idempotent — repeated calls return the cached
    /// [`CaLink`]. Creates a CA channel, subscribes a monitor, and
    /// spawns the task that keeps the cached snapshot current.
    ///
    /// This is the entry point an IOC calls at init for every CA
    /// record link so the synchronous resolver hot path can serve
    /// from cache (the C `dbCaAddLink` analogue).
    pub async fn open(&self, pv_name: &str) -> Result<Arc<CaLink>, CaLinkError> {
        if let Some(existing) = self.links.read().get(pv_name).cloned() {
            return Ok(existing);
        }
        let channel = Arc::new(self.client().await?.create_channel(pv_name));
        // subscribe the connection-event stream BEFORE the
        // `subscribe()` round-trip that drives the circuit connect, so the
        // watcher cannot miss the `Connected` event — that event is what
        // kicks off the one-shot CTRL attribute fetch (mirroring C
        // `connectionCallback` scheduling `CA_GET_ATTRIBUTES`). Subscribing
        // after `subscribe()` would race: a connect completing during the
        // await would emit `Connected` before the watcher existed, leaving
        // `meta` permanently empty until the next reconnect.
        let conn_rx = channel.connection_events();
        // C `dbCa` opens the record-link monitor with `ca_add_array_event`
        // (`dbCa.c:1258-1269`), whose libca macro expands to
        // `ca_add_masked_array_event(..., DBE_VALUE | DBE_ALARM)`
        // (`cadef.h:2004-2012`). DBE_LOG / DBE_ARCHIVE is a separate
        // event-trigger class (`cadef.h:1148-1158`) that `dbCa` never
        // requests, so a CP/CPP record link must not refresh its cache (or
        // wake a scan) on archive/log-only posts. The default
        // `CaChannel::subscribe()` requests `DBE_VALUE | DBE_LOG |
        // DBE_ALARM`, so request the dbCa mask explicitly here.
        let monitor = channel
            .subscribe_with_mask(0.0, CALINK_EVENT_MASK)
            .await
            .map_err(|e| CaLinkError::Subscribe {
                pv: pv_name.to_string(),
                reason: e.to_string(),
            })?;
        let cache: Arc<ArcSwap<Option<CachedSnapshot>>> = Arc::new(ArcSwap::from_pointee(None));
        let connected = Arc::new(LinkConnState::new(self.db.clone(), pv_name.to_string()));
        let meta: Arc<ArcSwap<Option<LinkMetadata>>> = Arc::new(ArcSwap::from_pointee(None));
        // Connection-event watcher: keeps `connected` in sync with the
        // real circuit state so `is_connected()` reflects upstream
        // disconnects (mirrors `pvalink`'s `monitor_connected` flag), and
        // re-fetches the remote CTRL attributes into `meta` on each
        // connect.
        let conn_task = task::spawn(run_connection_watcher(
            conn_rx,
            connected.clone(),
            channel.clone(),
            meta.clone(),
            pv_name.to_string(),
        ));
        let monitor_task = task::spawn(run_monitor(
            monitor,
            cache.clone(),
            connected.clone(),
            channel.clone(),
            pv_name.to_string(),
            self.db.clone(),
        ));
        let link = Arc::new(CaLink {
            cache,
            connected,
            meta,
            channel,
            _monitor_task: AbortOnDrop(monitor_task),
            _conn_task: AbortOnDrop(conn_task),
        });
        // Re-check under the write lock so two concurrent first-callers
        // converge on one link (the loser's freshly opened link drops,
        // unsubscribing its monitor).
        let mut links = self.links.write();
        if let Some(existing) = links.get(pv_name).cloned() {
            return Ok(existing);
        }
        links.insert(pv_name.to_string(), link.clone());
        Ok(link)
    }

    /// Wait until the CA link for `pv_name` has received at least one
    /// monitor event (its cached value is populated). Returns `false`
    /// on timeout. The canonical test / IOC-init helper for "wait for
    /// the upstream IOC to come online".
    pub async fn wait_for_link_connected(
        &self,
        pv_name: &str,
        timeout: std::time::Duration,
    ) -> bool {
        let name = strip_ca_scheme(pv_name);
        let link = match self.open(name).await {
            Ok(l) => l,
            Err(_) => return false,
        };
        let deadline = std::time::Instant::now() + timeout;
        loop {
            if link.value().is_some() {
                return true;
            }
            if std::time::Instant::now() >= deadline {
                return false;
            }
            // The `runtime::task` seam, not `tokio::time`: on the RTEMS
            // target this loop runs on the callback pool with no tokio
            // timer anywhere in the process, and a bare `tokio::time::sleep`
            // panics the band worker it is polled on.
            epics_base_rs::runtime::task::sleep(std::time::Duration::from_millis(25)).await;
        }
    }

    /// Number of open CA links.
    pub fn link_count(&self) -> usize {
        self.links.read().len()
    }

    /// C6 PROBE: every open link's PV name and connection state, sorted.
    ///
    /// The guest-side half of the on-target gate's criteria 1 and 4: with
    /// no iocsh on the target the console is the only place the link
    /// registry can be read from *inside* the IOC, next to what `caget`
    /// reads from outside. `dbcaxr` prints the same facts but needs a
    /// shell to invoke it.
    pub fn link_report(&self) -> Vec<(String, bool)> {
        let mut out: Vec<(String, bool)> = self
            .links
            .read()
            .iter()
            .map(|(name, link)| (name.clone(), link.is_connected()))
            .collect();
        out.sort();
        out
    }

    /// C6 PROBE: the shared client's virtual-circuit count, or `None`
    /// when no link has been opened yet and the lazy client does not
    /// exist.
    ///
    /// This is §2.4's "one upstream circuit regardless of link count",
    /// observed from the guest. The host-side counterpart is `ss -tn`
    /// against the upstream's port; both are recorded because either one
    /// alone can be read as an artefact of where it was measured.
    pub async fn client_connection_count(&self) -> Option<usize> {
        let client = self.client.get()?;
        Some(client.ioc_connection_count().await)
    }

    /// Lazily resolve `name` to its cached [`CaLink`]. Opens the link when
    /// it is not yet in the registry — the first-access slow path.
    /// Steady-state reads hit the registry directly.
    async fn link_for(&self, name: &str) -> Option<Arc<CaLink>> {
        if let Some(existing) = self.cached_link(name) {
            return Some(existing);
        }
        self.open(name).await.ok()
    }

    /// The registry read with NO lazy open — the record-processing half of
    /// [`Self::link_for`]. C `dbCaGetLink` and the `getAttributes` family
    /// read `pca->...` under `pca->lock` and never create a channel
    /// (`dbCa.c:448-535`, `:662-704`); the open is `dbCaAddLink`'s
    /// `CA_CONNECT` on the `dbCaTask`, reached here through
    /// [`LinkSet::connect_link`]. Every synchronous `LinkSet` accessor goes
    /// through this, so none of them can suspend the record thread.
    fn cached_link(&self, name: &str) -> Option<Arc<CaLink>> {
        self.links.read().get(name).cloned()
    }
}

/// Monitor task: drain the subscription, refresh the cache on every
/// event. Ends when the channel is dropped (`recv` returns `None`).
///
/// Mirrors C `dbCa.c` `eventCallback` (`dbCa.c:925`) — every CA
/// monitor event overwrites the cached value/severity/timestamp that
/// `dbCaGetLink` later serves.
async fn run_monitor(
    mut monitor: crate::client::MonitorHandle,
    cache: Arc<ArcSwap<Option<CachedSnapshot>>>,
    connected: Arc<LinkConnState>,
    channel: Arc<CaChannel>,
    pv_name: String,
    db: Arc<RwLock<Option<PvDatabase>>>,
) {
    while let Some(event) = monitor.recv().await {
        match event {
            Ok(snapshot) => {
                // A delivered monitor event is itself proof of
                // liveness — mark the link connected even if the
                // `Connected` lifecycle event has not been observed
                // yet (race-free, mirrors `pvalink`'s callback).
                connected.mark_connected();
                // Capture the channel's native element count this event was
                // produced under, so a later type/count change makes the
                // cache unservable (the DBR type is intrinsic to the
                // value). The channel is connected here — we just received
                // an event — so `element_count()` is `Ok`; fall back to the
                // payload count only if the description is momentarily
                // unavailable.
                let native_count = channel
                    .element_count()
                    .unwrap_or_else(|_| snapshot.value.count());
                cache.store(Arc::new(Some(CachedSnapshot {
                    snapshot,
                    native_count,
                })));
                // C `dbCa.c eventCallback` refreshes the cached value, then
                // adds `CA_DBPROCESS` for every CP link (and Passive CPP
                // link) on this PV (`dbCa.c:925,993-994`); the worker thread
                // later runs `db_process(prec)` (`dbCa.c:1295`). Drive the
                // Rust twin: process every local holder of an external
                // CP/CPP link to this PV. The cache is stored ABOVE first,
                // so the holder's INP read sees the fresh value — matching
                // the C ordering (cache update precedes the process request).
                // No-op when no holder is registered or the DB is not
                // attached. The read
                // guard is dropped before the await so the lock is never
                // held across the process call.
                let db_handle = db.read().clone();
                if let Some(db_handle) = db_handle {
                    db_handle.dispatch_external_cp_targets(&pv_name);
                }
            }
            // A monitor error event (e.g. a transient server-side
            // problem) leaves the last cached value in place — the
            // next good event refreshes it. C `dbCa.c` keeps the
            // stale value on a monitor error the same way.
            Err(e) => {
                tracing::debug!(
                    pv = %pv_name,
                    error = %e,
                    "calink: monitor error event ignored, keeping last cached value"
                );
            }
        }
    }
    // Subscription ended (channel dropped). Reflect the disconnect —
    // through the owner, so the CP holders are processed here too.
    connected.mark_disconnected();
}

/// Connection-event watcher: keep `connected` in sync with the CA
/// circuit state. `Connected` flips it `true`; `Disconnected` /
/// `Unresponsive` flip it `false` so a downstream IOC restart is
/// reflected by `CaLink::is_connected()`. Mirrors `dbCa.c`'s
/// `connectionCallback` setting `pca->connected`.
///
/// on every `Connected` the watcher also (re)fetches the
/// remote CTRL attributes into `meta`, mirroring `connectionCallback`
/// scheduling `CA_GET_ATTRIBUTES` (`dbCa.c:910`). The fetch is detached
/// so a slow or hung CTRL get never delays the watcher from observing a
/// later disconnect — the metadata is best-effort and the read path
/// gates on `connected` regardless.
async fn run_connection_watcher(
    mut conn_rx: epics_base_rs::runtime::sync::broadcast::Receiver<crate::client::ConnectionEvent>,
    connected: Arc<LinkConnState>,
    channel: Arc<CaChannel>,
    meta: Arc<ArcSwap<Option<LinkMetadata>>>,
    pv_name: String,
) {
    loop {
        match conn_rx.recv().await {
            // A fresh `Connected` transition kicks off the CTRL attribute
            // refetch; detached so a hung get never stalls
            // the watcher from seeing a later disconnect.
            Ok(evt) => {
                if note_conn_event(&evt, &connected) {
                    task::spawn(fetch_link_metadata(
                        channel.clone(),
                        meta.clone(),
                        pv_name.clone(),
                    ));
                }
            }
            // Lagged: a burst of events overran the bounded channel.
            // Keep watching; the next event resyncs the flag.
            Err(epics_base_rs::runtime::sync::broadcast::error::RecvError::Lagged(_)) => continue,
            // Closed: the channel was dropped — watcher's job is done.
            Err(epics_base_rs::runtime::sync::broadcast::error::RecvError::Closed) => return,
        }
    }
}

/// Apply one connection event to the [`LinkConnState`] owner, returning
/// `true` iff this was a `Connected` transition (the caller then kicks
/// off a metadata refetch). `Disconnected` clears the flag — an echo
/// timeout arrives as `Disconnected` too, exactly as `CA_OP_CONN_DOWN`
/// does in C. `AccessRightsChanged` never touches the flag; it routes
/// into [`LinkConnState::note_access_rights`] — the C
/// `accessRightsCallback` (`dbCa.c:1076-1102`), which caches the read
/// right and dispatches the CP/CPP holders on a rights loss while
/// connected. `NativeTypeChanged` leaves the state untouched.
/// Factored out of [`run_connection_watcher`] so the transition logic —
/// the disconnect-tracking regression — is unit-testable without a live
/// CA channel.
fn note_conn_event(evt: &crate::client::ConnectionEvent, state: &LinkConnState) -> bool {
    use crate::client::ConnectionEvent;
    match evt {
        ConnectionEvent::Connected => {
            state.mark_connected();
            true
        }
        ConnectionEvent::Disconnected => {
            state.mark_disconnected();
            false
        }
        ConnectionEvent::AccessRightsChanged { read, write } => {
            state.note_access_rights(*read, *write);
            false
        }
        _ => false,
    }
}

/// One-shot CTRL attribute fetch for a CA link, mirroring C `dbCa.c`'s
/// `CA_GET_ATTRIBUTES` → `getAttribEventCallback` (`dbCa.c:1249`,
/// `:1080`): a `DBR_CTRL` get whose reply fills the cached
/// control/display/alarm limits, precision and units. The channel's
/// native DBF type and element count come from the channel info (C
/// `getDBFtype`/`getElements` read `pca->dbrType`/`nelements`, not the
/// CTRL reply). Best-effort: on a failed CTRL get the type/count are
/// still stored and the next reconnect retries the limits.
async fn fetch_link_metadata(
    channel: Arc<CaChannel>,
    meta: Arc<ArcSwap<Option<LinkMetadata>>>,
    pv_name: String,
) {
    // DBF type + element count from the channel's native description.
    let (dbf, element_count) = match channel.info().await {
        Ok(info) => (Some(info.native_type), Some(info.element_count)),
        Err(_) => (None, None),
    };
    // Limits/precision/units from a single DBR_CTRL get, at a FIXED
    // `DBR_CTRL_DOUBLE` and gated on the native type — both straight from C.
    // `dbCa.c:926-928` asks for attributes for every channel whose
    // `pca->dbrType` is not `DBR_STRING`, and `:1275` issues that get as
    // `ca_get_callback(DBR_CTRL_DOUBLE, ...)` whatever the native type is, so
    // the server converts and `gotAttributes` goes TRUE for an ENUM target
    // too. Requesting the NATIVE CTRL type instead put a `DBR_CTRL_ENUM` on
    // the wire for an enum channel — a struct with no precision, units or
    // limit members at all (`db_access.h struct dbr_ctrl_enum`) — so every
    // attribute stayed `None` where C serves precision 0, empty units and
    // zeroed limits. Count 1: the attributes live in the metadata header, so
    // there is no need to pull a whole waveform for them.
    let attrs = if dbf == Some(DbFieldType::String) {
        // C never issues the get for a string channel; `gotAttributes` stays
        // FALSE and every getter returns -1. Skipping it is that gate.
        None
    } else {
        match channel.get_with_dbr_type(DBR_CTRL_DOUBLE, 1).await {
            Ok(snap) => Some(snap),
            Err(e) => {
                tracing::debug!(
                    pv = %pv_name,
                    error = %e,
                    "calink: CTRL attribute get failed; serving DBF type / element count only"
                );
                None
            }
        }
    };
    // The enum state-label table is this port's own cache and has no dbCa
    // analogue on this path — C renders a remote enum as text through a
    // second `DBR_STRING` monitor (`pgetString`), not through the attribute
    // get. It needs the native `DBR_CTRL_ENUM` reply, which the fixed-DOUBLE
    // get above cannot carry, so it rides its own request.
    let labels = if dbf == Some(DbFieldType::Enum) {
        channel
            .get_with_metadata_count(DbrClass::Ctrl, 1)
            .await
            .inspect_err(|e| {
                tracing::debug!(
                    pv = %pv_name,
                    error = %e,
                    "calink: enum label get failed; serving no choice table"
                );
            })
            .ok()
    } else {
        None
    };
    meta.store(Arc::new(Some(build_link_metadata(
        dbf,
        element_count,
        attrs.as_ref(),
        labels.as_ref(),
    ))));
}

/// Map the fetched attribute [`Snapshot`] plus the channel's native DBF
/// type / element count into a [`LinkMetadata`]. Pure transform, factored
/// out of [`fetch_link_metadata`] so the field mapping is unit-testable
/// without a live CA server.
///
/// The two snapshots are different requests and each carries exactly one
/// thing: `attrs` is the fixed `DBR_CTRL_DOUBLE` attribute get and is the
/// only source of limits/precision/units; `labels` is the native
/// `DBR_CTRL_ENUM` get an enum channel also gets, and is the only source of
/// the choice table.
///
/// A `None` attribute field means the source carried nothing, and only a
/// DBR_STRING channel reaches that state for the whole set: C never issues
/// the get for one (`dbCa.c:926-928`), `pca->gotAttributes` stays FALSE and
/// `getPrecision`/`getUnits`/the limit getters return -1 with the caller's
/// buffer untouched. An ENUM channel is NOT that case — C's get is a fixed
/// `DBR_CTRL_DOUBLE` (`:1275`), the server converts, `gotAttributes` goes
/// TRUE, and the getters SUCCEED with precision 0, empty units and zeroed
/// limits. Alarm-limit order is `(lolo, lo, hi, hihi)`, matching C
/// `getAlarmLimits` (`dbCa.c:758`).
fn build_link_metadata(
    dbf: Option<DbFieldType>,
    element_count: Option<u32>,
    attrs: Option<&Snapshot>,
    labels: Option<&Snapshot>,
) -> LinkMetadata {
    let mut md = LinkMetadata {
        dbf_type: dbf.map(map_dbf_type),
        element_count: element_count.map(|n| n as i64),
        ..LinkMetadata::default()
    };
    if let Some(snap) = attrs {
        if let Some(d) = snap.display.as_ref() {
            md.graphic_limits = Some((d.lower_disp_limit, d.upper_disp_limit));
            md.alarm_limits = Some((
                d.lower_alarm_limit,
                d.lower_warning_limit,
                d.upper_warning_limit,
                d.upper_alarm_limit,
            ));
            md.precision = Some(d.precision);
            if !d.units.is_empty() {
                // `LinkMetadata` is the dbCa client-side metadata cache, a
                // separate representation from the byte-preserving wire
                // encoders; it keeps a `String` rendering of the units (as
                // lossy here as the previous decode was).
                md.units = Some(d.units.as_str_lossy().into_owned());
            }
            if !d.description.is_empty() {
                // Same lossy `String` cache rendering as `units` above.
                md.description = Some(d.description.as_str_lossy().into_owned());
            }
        }
        if let Some(c) = snap.control.as_ref() {
            md.control_limits = Some((c.lower_ctrl_limit, c.upper_ctrl_limit));
        }
    }
    // An enum channel's native CTRL reply is `DBR_CTRL_ENUM`, whose metadata
    // IS the state-label table. Cached so a `DBR_STRING`-requesting reader
    // (stringin/lsi INP, printf `%s`) renders the remote index as its label —
    // C `dbCa` keeps a second `DBR_STRING` monitor (`pgetString`) for that
    // read. It is a separate request from the attribute get, which is a fixed
    // DBR_CTRL_DOUBLE and carries no labels.
    if let Some(e) = labels.and_then(|s| s.enums.as_ref())
        && !e.strings.is_empty()
    {
        md.enum_choices = Some(
            e.strings
                .iter()
                .map(|s| s.as_str_lossy().into_owned())
                .collect(),
        );
    }
    md
}

/// Map a CA native [`DbFieldType`] to the link-metadata
/// [`LinkDbfType`]. Mirrors C `getDBFtype` returning
/// `dbDBRoldToDBFnew[pca->dbrType]` (`dbCa.c:695`). The CA wire protocol
/// carries only the seven base types; `Int64`/`UInt64` are internal and
/// never appear over CA (such PVs present as `Double`), but are mapped
/// for completeness.
fn map_dbf_type(t: DbFieldType) -> LinkDbfType {
    match t {
        DbFieldType::String => LinkDbfType::String,
        DbFieldType::Short => LinkDbfType::Short,
        DbFieldType::Float => LinkDbfType::Float,
        DbFieldType::Enum => LinkDbfType::Enum,
        DbFieldType::Char => LinkDbfType::Char,
        DbFieldType::Long => LinkDbfType::Long,
        DbFieldType::Double => LinkDbfType::Double,
        DbFieldType::Int64 => LinkDbfType::Int64,
        DbFieldType::UInt64 => LinkDbfType::UInt64,
        // DBF_USHORT/DBF_ULONG are internal like Int64/UInt64 and never
        // appear over CA (they present as DBR_LONG / DBR_DOUBLE); mapped
        // for completeness.
        DbFieldType::UShort => LinkDbfType::UShort,
        DbFieldType::ULong => LinkDbfType::ULong,
        // DBF_UCHAR presents as DBR_CHAR over CA but keeps its own link type.
        DbFieldType::UChar => LinkDbfType::UChar,
    }
}

#[async_trait::async_trait]
impl LinkSet for CaLinkResolver {
    fn is_connected(&self, name: &str) -> bool {
        let name = strip_ca_scheme(name);
        match self.links.read().get(name) {
            Some(link) => link.is_connected(),
            // Not opened yet — report not connected. `open` /
            // `get_value` open it lazily.
            None => false,
        }
    }

    fn init_ready(&self, name: &str) -> bool {
        let name = strip_ca_scheme(name);
        match self.links.read().get(name) {
            Some(link) => link.init_ready(),
            None => false,
        }
    }

    async fn get_value(&self, name: &str) -> Option<EpicsValue> {
        let name = strip_ca_scheme(name);
        self.link_for(name).await?.value()
    }

    /// C `dbCaGetLink` (`dbCa.c:448-535`): copy out of the buffer the CA
    /// monitor keeps fresh, never open and never wait. Here that buffer is
    /// [`CaLink::value`], refreshed by `run_monitor` on every subscription
    /// event (the `eventCallback` analogue, `dbCa.c:925`).
    ///
    /// The difference from [`Self::get_value`] is the missing
    /// `Self::link_for` fallback (`resolver.rs:452-457`), which opens the
    /// channel and awaits the subscription round trip. That open now happens
    /// on the database's link work owner via [`Self::connect_link`].
    fn get_cached_value(&self, name: &str) -> Option<EpicsValue> {
        let name = strip_ca_scheme(name);
        let link = self.links.read().get(name).cloned()?;
        link.value()
    }

    /// C `dbCaAddLink`'s `CA_CONNECT` work (`dbCa.c:735-800`): create the
    /// channel and its monitor. Runs on the link work owner, so the
    /// `subscribe` round trip inside `open` is off the record thread.
    async fn connect_link(&self, name: &str) {
        let name = strip_ca_scheme(name);
        let _ = self.open(name).await;
    }

    /// C `dbCaPutLinkCallback`'s `if (!pca->isConnected || !pca->hasWriteAccess)
    /// return -1;` (`dbCa.c:558-561`), answered from cached state: the links
    /// map plus the `CaLink::connected` owner, which carries both the
    /// connection flag (`note_conn_event`) and the cached rights
    /// (`note_access_rights`). No I/O — the database asks this on the
    /// record-processing thread, inside the record's advisory write gate.
    ///
    /// BOTH of C's operands are tested here, not just the first. A
    /// write-denied but connected link that passed this gate was staged and
    /// then refused on the link work owner, one full record cycle late: the
    /// owning record stayed NO_ALARM where C shows LINK/INVALID every
    /// cycle, and the put-notify flavour resolved its wait-set as a success
    /// because the completion channel carries no status back to the record.
    /// Both flavours are closed by refusing here; neither needs a second,
    /// later check.
    ///
    /// A link this resolver has never opened reports `Unopened` rather than
    /// `Refused`: C opens at `dbCaAddLink` (record init) so the `caLink`
    /// always exists by the first put, while this resolver opens lazily
    /// inside `put_value` / `get_value`. Reporting `Refused` would refuse
    /// the very write whose staging performs the open, and the link would
    /// never connect.
    fn put_admission(&self, name: &str) -> PutAdmission {
        let name = strip_ca_scheme(name);
        let links = self.links.read();
        match links.get(name) {
            None => PutAdmission::Unopened,
            Some(link) if link.is_connected() && link.has_write_access() => PutAdmission::Connected,
            Some(_) => PutAdmission::Refused,
        }
    }

    async fn put_value(&self, name: &str, value: EpicsValue, op: LinkPutOp) -> Result<(), String> {
        // Honour the C dbCore split between a plain link put and a
        // put-notify-aware put. `dbCaPutLink` (no callback) sets
        // `putType = CA_PUT`, and the CA task later issues a
        // fire-and-forget `ca_array_put` — the source record's
        // processing does NOT block on the remote put completing
        // (dbCa.c:627-633 `dbCaPutLink`, dbCa.c:1201-1206 the
        // `CA_PUT` dispatch). Only `dbCaPutLinkCallback`
        // (`putType = CA_PUT_CALLBACK`) issues `ca_array_put_callback`
        // and parks the originating record until completion. The
        // database maps a plain record-processing OUT write to
        // `LinkPutOp::Plain` and a put-notify / blocking-put chain
        // write to `LinkPutOp::Async`, so the resolver must preserve
        // the split: routing `Plain` through the fire-and-forget
        // `CA_PROTO_WRITE` (`put_nowait`) keeps a slow or hung remote
        // CA server from stalling normal record processing for the
        // `EPICS_CA_PUT_TIMEOUT` window, while `Async` keeps the
        // WRITE_NOTIFY completion wait the put-notify chain needs.
        let name = strip_ca_scheme(name);
        let link = self
            .link_for(name)
            .await
            .ok_or_else(|| format!("CA link {name} not open"))?;
        let channel = link.channel.clone();
        // The clamp C applies before the CA request is ever built:
        // `dbCaPutLinkCallback` converts the caller's buffer to the
        // channel's native type and bounds the request to the channel's
        // element count in the same step — `if(nRequest>pca->nelements)
        // nRequest = pca->nelements;` then `aConvert(..., nRequest,
        // pca->nelements, 0)` (`dbCa.c:604-606`), against
        // `pca->nelements = ca_element_count(chid)` (`:906`). The surplus
        // elements are DROPPED and the put succeeds; the same rule holds
        // for a DB target (`dbAccess.c:1365`), so an oversized array put
        // behaves identically whether the link is CA or DB.
        //
        // It has to happen HERE and not in `CaChannel::put`, because
        // libca genuinely refuses an oversized direct `ca_array_put`:
        // `nciu::write` throws `outOfBounds` on `countIn > this->count`
        // (`nciu.cpp:332-334`) and `ca_array_put` maps it to
        // ECA_BADCOUNT (`oldChannelNotify.cpp:512`). C reaches that only
        // from a direct client write, never from a link put — dbCa has
        // already clamped. Clamping the converted value keeps
        // `validate_put_count` unreachable from this path by
        // construction rather than by a second bound inside the client.
        let value = match (channel.native_field_type(), channel.element_count()) {
            (Ok(native), Ok(nelements)) => {
                let mut converted = value.convert_to(native);
                converted.truncate(nelements as usize);
                converted
            }
            // No native description yet: the channel is not operational,
            // so there is nothing to clamp against and the write below
            // reports the disconnect itself.
            _ => value,
        };
        match op {
            LinkPutOp::Plain => channel.put_nowait(&value).await,
            LinkPutOp::Async => channel.put(&value).await,
        }
        .map_err(|e| e.to_string())
    }

    fn alarm_severity(&self, name: &str) -> Option<i32> {
        let name = strip_ca_scheme(name);
        let sev = self.cached_link(name)?.alarm_severity()?;
        // Mirror the lset contract: only a non-zero severity is a
        // contribution worth propagating into the owning record's
        // LINK_ALARM. `0` (NO_ALARM) means "do not propagate".
        if sev > 0 { Some(sev) } else { None }
    }

    fn alarm_status(&self, name: &str) -> Option<i32> {
        // surface the remote STAT for `MSS` propagation.
        // Record processing only consults this when the alarm is
        // actually propagated (severity > 0 via `alarm_severity`), so
        // no severity gate is needed here.
        let name = strip_ca_scheme(name);
        self.cached_link(name)?.alarm_status()
    }

    fn time_stamp(&self, name: &str) -> Option<(i64, i32, u64)> {
        let name = strip_ca_scheme(name);
        self.cached_link(name)?.time_stamp()
    }

    fn link_metadata(&self, name: &str) -> Option<LinkMetadata> {
        // surface the remote display/control/alarm limits,
        // precision, units, DBF type and element count through the DB
        // link API so a record with a CA INP link inherits them, matching
        // the pvalink metadata path. Reads the cached CTRL attributes —
        // no fresh GET — exactly as C `getControlLimits` &c. read
        // `pca->controlLimits`.
        let name = strip_ca_scheme(name);
        self.cached_link(name)?.link_metadata()
    }

    fn link_names(&self) -> Vec<String> {
        self.links.read().keys().cloned().collect()
    }
}

/// Strip a leading `ca://` scheme prefix. `epics-base-rs` stores both
/// the scheme-prefixed and the bare form in `ParsedLink::Ca`
/// depending on the link syntax (`ca://X` vs the bare ` CA` modifier),
/// so the resolver normalises to the bare PV name.
fn strip_ca_scheme(name: &str) -> &str {
    name.strip_prefix("ca://").unwrap_or(name)
}

/// Install a [`CaLinkResolver`] on `db`, registered under the `"ca"`
/// lset scheme. After this, a record whose link field resolves to
/// `ParsedLink::Ca` (a `ca://X` link or a bare ` CA`-modified link)
/// reads through the monitor-backed CA cache via
/// `PvDatabase::resolve_external_pv`.
///
/// Returns the resolver so the caller can pre-open links
/// ([`CaLinkResolver::open`]) at IOC init and query stats.
///
/// Infallible: the shared CA client is created lazily on the first link
/// open ([`CaLinkResolver::new`]), so installation never spins one up
/// and never fails — a database with no CA links is fully serviceable
/// and an IOC must not be aborted by CA-client init. This is the
/// CA-side twin of the bridge's infallible `install_pvalink_resolver`.
pub async fn install_calink_resolver(db: &PvDatabase) -> CaLinkResolver {
    let resolver = CaLinkResolver::new();
    // Attach the DB before registering the lset, so the monitor callback
    // can drive `dispatch_external_cp_targets` from the first event on.
    // This runs before iocInit's `setup_cp_links` warms any external CP
    // link, so the handle is always
    // present when the first warmed-link event arrives.
    resolver.attach_database(db.clone());
    db.register_link_set("ca", Arc::new(resolver.clone())).await;
    resolver
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::ConnectionEvent;

    #[test]
    fn strip_ca_scheme_handles_both_forms() {
        assert_eq!(strip_ca_scheme("ca://OTHER:PV"), "OTHER:PV");
        assert_eq!(strip_ca_scheme("OTHER:PV"), "OTHER:PV");
    }

    /// The CA record-link monitor mask matches C `dbCa`'s
    /// `ca_add_array_event` = `DBE_VALUE | DBE_ALARM` and EXCLUDES
    /// `DBE_LOG` (`cadef.h:2004-2012` / `:1148-1158`). `open()` subscribes
    /// with this exact constant, so an archive/log-only upstream post does
    /// not refresh a CP/CPP link's cache. Guards against a regression back
    /// to the default `subscribe()` mask, which adds `DBE_LOG`.
    #[test]
    fn calink_monitor_mask_is_dbca_value_alarm_without_log() {
        use crate::protocol::{DBE_ALARM, DBE_LOG, DBE_VALUE};
        assert_eq!(
            CALINK_EVENT_MASK,
            DBE_VALUE | DBE_ALARM,
            "calink monitor mask must equal dbCa's DBE_VALUE | DBE_ALARM"
        );
        assert_eq!(
            CALINK_EVENT_MASK & DBE_LOG,
            0,
            "calink must not request DBE_LOG — dbCa.c never does"
        );
    }

    /// BUG 1 regression: the connection-event → `connected` flag
    /// transition, which [`note_conn_event`] owns (the watcher loop only
    /// calls it). A disconnect MUST flip the flag false; pre-fix
    /// `is_connected()` keyed off cache presence alone and stayed `true`
    /// forever once any event had been cached, so an upstream IOC restart
    /// was invisible and stale data was served. a
    /// `Connected` transition additionally returns `true` to signal the
    /// CTRL attribute refetch; the clearing/neutral events return `false`.
    #[test]
    fn bug1_connection_event_tracks_disconnect() {
        // No database attached: `mark_disconnected`'s dispatch is a no-op,
        // so this test stays about the flag alone. The dispatch itself is
        // covered by `disconnect_edge_dispatches_cp_holders_once`.
        let connected = LinkConnState::new(Arc::new(RwLock::new(None)), "UP:PV".to_string());

        // Circuit comes up — flag true, and signals an attribute refetch.
        assert!(note_conn_event(&ConnectionEvent::Connected, &connected));
        assert!(connected.is_set());

        // Upstream IOC restart — circuit drops. Flag MUST go false; no
        // refetch on a disconnect.
        assert!(!note_conn_event(&ConnectionEvent::Disconnected, &connected));
        assert!(!connected.is_set());

        // Reconnect — flag true again (CA monitors auto-restore), refetch
        // signalled again.
        assert!(note_conn_event(&ConnectionEvent::Connected, &connected));
        assert!(connected.is_set());

        // Echo timeout (TCP up, server hung) reaches the watcher as a plain
        // `Disconnected` — C's `unresponsiveCircuitNotify` fires the same
        // `CA_OP_CONN_DOWN` — so it clears the flag with no refetch.
        assert!(!note_conn_event(&ConnectionEvent::Disconnected, &connected));
        assert!(!connected.is_set());

        // An access-rights change never touches the connection flag and
        // signals no refetch — but it is not neutral: it routes into
        // `note_access_rights` (the C `accessRightsCallback`), which
        // caches the read right. The flag staying set while the read
        // gate closes is exactly the read-denied-but-connected state.
        connected.mark_connected();
        assert!(!note_conn_event(
            &ConnectionEvent::AccessRightsChanged {
                read: false,
                write: true,
            },
            &connected,
        ));
        assert!(connected.is_set());
        assert!(
            !connected.has_read_access(),
            "the watcher must route AccessRightsChanged into note_access_rights"
        );
    }

    /// §11.7 item 1: C `accessRightsCallback` (`dbCa.c:1076-1102`) as the
    /// owner decides it, per invariant boundary — not per narrative:
    ///
    /// * rights event while DISCONNECTED → no dispatch AND no flag update
    ///   (`dbCa.c:1084-1085` returns before touching the cache;
    ///   "connectionCallback will handle"), so a stale rights event queued
    ///   behind a `Disconnected` cannot double-dispatch the outage;
    /// * read lost while connected → dispatch, read gate closes;
    /// * write lost ALONE while connected → dispatch too: the C gate is
    ///   `if (hasReadAccess && hasWriteAccess) goto done` (`dbCa.c:1091`),
    ///   i.e. C processes on the loss of EITHER right, not read alone —
    ///   but the read gate stays open, so the dispatched holder reads a
    ///   good value and lands no alarm;
    /// * full regain → NO dispatch (`dbCa.c:1091`): the holder's alarm
    ///   clears on the next monitor event, not on the rights edge.
    #[test]
    fn access_rights_transitions_follow_dbca() {
        let state = LinkConnState::new(Arc::new(RwLock::new(None)), "UP:PV".to_string());

        // Disconnected: C's early return. No dispatch, no cache update.
        assert!(!state.note_access_rights(false, false));
        assert!(
            state.has_read_access(),
            "rights must not change while disconnected — the reconnect \
             re-delivers the real rights right after Connected"
        );

        assert!(state.mark_connected());

        // Read lost while connected: dispatch, and the value gate closes.
        assert!(state.note_access_rights(false, true));
        assert!(!state.has_read_access());

        // A further change while still degraded (now both lost): C runs
        // accessRightsCallback once per rights change and dispatches each
        // time the rights are not fully held — per change, not per edge.
        assert!(state.note_access_rights(false, false));
        assert!(!state.has_read_access());

        // Full regain: no dispatch, gate reopens.
        assert!(!state.note_access_rights(true, true));
        assert!(state.has_read_access());

        // Write lost alone: dispatch (dbCa.c:1091), read gate stays open.
        assert!(state.note_access_rights(true, false));
        assert!(state.has_read_access());

        // The disconnect edge owns its own dispatch; a rights event
        // arriving after it is the disconnected early-return again — one
        // outage, one dispatch.
        assert!(state.mark_disconnected());
        assert!(!state.note_access_rights(false, false));
    }

    /// Stage C6 criterion 4 regression, at the level the invariant is
    /// stated: only the true→false EDGE is a disconnect, so the CP-holder
    /// dispatch happens exactly once per outage no matter how many
    /// `Disconnected` events and subscription-end tails arrive.
    ///
    /// The dispatch's own effect (holder processes, link read fails,
    /// LINK/INVALID commits) is a database behaviour and is covered on the
    /// `epics-base-rs` side; what can only be checked here is that the
    /// transition owner is the thing that decides.
    #[test]
    fn disconnect_edge_dispatches_cp_holders_once() {
        let state = LinkConnState::new(Arc::new(RwLock::new(None)), "UP:PV".to_string());

        // Never connected: a disconnect is not an edge and must not
        // dispatch — otherwise every link would process its holders once
        // at startup, before any value existed.
        assert!(!state.mark_disconnected());

        assert!(state.mark_connected());
        // Already connected: not an edge.
        assert!(!state.mark_connected());

        // The outage. One edge...
        assert!(state.mark_disconnected());
        // ...and every repeat (watcher event, then `run_monitor`'s
        // subscription-ended tail) is not.
        assert!(!state.mark_disconnected());
        assert!(!state.mark_disconnected());

        assert!(state.mark_connected());
        assert!(state.mark_disconnected());
    }

    /// BUG 1 regression: the `is_connected()` / `value()` gating logic
    /// itself — a link with a populated cache but a `false` connected
    /// flag must report not-connected and serve no value. This is the
    /// state during an upstream outage (cache holds the last
    /// Snapshot, circuit is down).
    #[test]
    fn bug1_disconnected_link_serves_no_stale_value() {
        // Reproduce the exact gate `is_connected()` / `value()` apply:
        // both require `connected == true`.
        let cache: Arc<ArcSwap<Option<Snapshot>>> = Arc::new(ArcSwap::from_pointee(None));
        let connected = Arc::new(AtomicBool::new(false));

        // Populate the cache with a stale snapshot, circuit still down.
        cache.store(Arc::new(Some(Snapshot::new(
            EpicsValue::Double(42.0),
            0,
            0,
            std::time::SystemTime::UNIX_EPOCH,
        ))));

        // Gate: cache is present but connected is false.
        let is_connected = connected.load(Ordering::Acquire) && cache.load().as_ref().is_some();
        assert!(
            !is_connected,
            "a disconnected link must report not-connected even with a cached snapshot"
        );
        let value = if connected.load(Ordering::Acquire) {
            cache.load().as_ref().as_ref().map(|s| s.value.clone())
        } else {
            None
        };
        assert!(
            value.is_none(),
            "a disconnected link must serve no stale value"
        );

        // Circuit comes back — both gates open.
        connected.store(true, Ordering::Release);
        let is_connected = connected.load(Ordering::Acquire) && cache.load().as_ref().is_some();
        assert!(
            is_connected,
            "reconnected link with cache must be connected"
        );
    }

    /// BRIDGE-106 regression: after an upstream reconnect changes the DBR
    /// type or element count, the snapshot cached under the old description
    /// is no longer servable (so `value()`/`is_connected()` report nothing
    /// until a new monitor event repopulates a matching cache). Mirrors C
    /// `dbCa.c:865-889` / the `dbCaGetLink` invalid-cache path
    /// (`dbCa.c:484-492`). Tests the type/count gate directly — the
    /// live-channel `cache_matches_channel` is a thin wrapper. By invariant
    /// boundary: unchanged, DBR-type-changed, element-count-changed.
    #[test]
    fn calink_cache_invalidated_on_native_type_or_count_change() {
        // Unchanged description ⇒ still servable.
        assert!(
            cache_native_matches(DbFieldType::Double, 1, DbFieldType::Double, 1),
            "matching scalar type+count stays servable"
        );
        assert!(
            cache_native_matches(DbFieldType::Short, 10, DbFieldType::Short, 10),
            "matching waveform type+count stays servable"
        );
        // DBR type changed (Short -> Double), same count ⇒ unservable.
        assert!(
            !cache_native_matches(DbFieldType::Short, 1, DbFieldType::Double, 1),
            "a DBR-type change invalidates the old cache"
        );
        // Element count changed (NELM 10 -> 5), same type ⇒ unservable.
        assert!(
            !cache_native_matches(DbFieldType::Short, 10, DbFieldType::Short, 5),
            "an element-count change invalidates the old cache"
        );
    }

    /// Build a CTRL [`Snapshot`] carrying display + control metadata,
    /// the shape `get_with_metadata(DbrClass::Ctrl)` returns for a
    /// numeric PV.
    fn ctrl_snapshot() -> Snapshot {
        use epics_base_rs::server::snapshot::{ControlInfo, DisplayInfo};
        let mut snap = Snapshot::new(
            EpicsValue::Double(0.0),
            0,
            0,
            std::time::SystemTime::UNIX_EPOCH,
        );
        snap.display = Some(DisplayInfo {
            units: "degC".into(),
            precision: 3,
            upper_disp_limit: 100.0,
            lower_disp_limit: -50.0,
            upper_alarm_limit: 90.0,    // hihi
            upper_warning_limit: 80.0,  // hi
            lower_warning_limit: -20.0, // lo
            lower_alarm_limit: -40.0,   // lolo
            ..Default::default()
        });
        snap.control = Some(ControlInfo {
            upper_ctrl_limit: 95.0,
            lower_ctrl_limit: -45.0,
        });
        snap
    }

    /// a numeric PV's CTRL attributes map into every
    /// `LinkMetadata` field. Pins the alarm-limit order to
    /// `(lolo, lo, hi, hihi)` (C `getAlarmLimits`), graphic/control to
    /// `(lower, upper)`, and the channel info to dbf type + element
    /// count.
    #[test]
    fn build_link_metadata_numeric_maps_all_fields() {
        let snap = ctrl_snapshot();
        let md = build_link_metadata(Some(DbFieldType::Double), Some(1), Some(&snap), None);
        assert_eq!(md.dbf_type, Some(LinkDbfType::Double));
        assert_eq!(md.element_count, Some(1));
        assert_eq!(md.graphic_limits, Some((-50.0, 100.0)));
        assert_eq!(md.control_limits, Some((-45.0, 95.0)));
        assert_eq!(md.alarm_limits, Some((-40.0, -20.0, 80.0, 90.0)));
        assert_eq!(md.precision, Some(3));
        assert_eq!(md.units.as_deref(), Some("degC"));
    }

    /// a String channel gets no attribute request at all — C
    /// `dbCa.c:926-928` sets `CA_GET_ATTRIBUTES` only when
    /// `pca->dbrType != DBR_STRING`, so `gotAttributes` stays FALSE and
    /// every getter returns -1 with the caller's buffer untouched. Here
    /// that is `attrs = None`, and every limit stays `None` so the owning
    /// record keeps its local default.
    #[test]
    fn build_link_metadata_string_pv_has_no_limits() {
        let md = build_link_metadata(Some(DbFieldType::String), Some(1), None, None);
        assert_eq!(md.dbf_type, Some(LinkDbfType::String));
        assert_eq!(md.element_count, Some(1));
        assert_eq!(md.graphic_limits, None);
        assert_eq!(md.control_limits, None);
        assert_eq!(md.alarm_limits, None);
        assert_eq!(md.precision, None);
        assert_eq!(md.units, None);
    }

    /// an enum channel's `DBR_CTRL_ENUM` reply carries the state-label
    /// table; it lands in `enum_choices` so a `DBR_STRING`-requesting
    /// reader (stringin/lsi INP, printf `%s`) can render the remote index
    /// as its label — C `dbCa`'s `pgetString` monitor equivalent. An empty
    /// table stays `None` (nothing to render through).
    #[test]
    fn build_link_metadata_enum_ctrl_carries_the_choices() {
        use epics_base_rs::server::snapshot::EnumInfo;
        let mut snap = Snapshot::new(EpicsValue::Enum(1), 0, 0, std::time::SystemTime::UNIX_EPOCH);
        snap.enums = Some(EnumInfo::new(vec!["off".into(), "on".into()]));
        let md = build_link_metadata(Some(DbFieldType::Enum), Some(1), None, Some(&snap));
        assert_eq!(md.dbf_type, Some(LinkDbfType::Enum));
        assert_eq!(
            md.enum_choices,
            Some(vec!["off".to_string(), "on".to_string()])
        );

        let empty = Snapshot::new(EpicsValue::Enum(0), 0, 0, std::time::SystemTime::UNIX_EPOCH);
        let md = build_link_metadata(Some(DbFieldType::Enum), Some(1), None, Some(&empty));
        assert_eq!(md.enum_choices, None);

        // The two replies compose: the fixed DBR_CTRL_DOUBLE attribute get
        // carries an enum target's precision/units/limits (all zero, as C's
        // does) while the native DBR_CTRL_ENUM get carries the labels.
        use epics_base_rs::server::snapshot::DisplayInfo;
        let mut attrs = Snapshot::new(
            EpicsValue::Double(1.0),
            0,
            0,
            std::time::SystemTime::UNIX_EPOCH,
        );
        attrs.display = Some(DisplayInfo::default());
        let md = build_link_metadata(Some(DbFieldType::Enum), Some(1), Some(&attrs), Some(&snap));
        assert_eq!(md.precision, Some(0));
        assert_eq!(md.graphic_limits, Some((0.0, 0.0)));
        assert_eq!(
            md.enum_choices,
            Some(vec!["off".to_string(), "on".to_string()])
        );
    }

    /// when the channel info fetch failed (`None` dbf /
    /// count) and there is no CTRL reply, every field is `None` — the
    /// link reports "no metadata yet" rather than fabricating zeros.
    #[test]
    fn build_link_metadata_no_info_no_ctrl_is_all_none() {
        let md = build_link_metadata(None, None, None, None);
        assert_eq!(md, LinkMetadata::default());
    }

    /// an empty `units` string is dropped to `None` (the
    /// remote carried no engineering units), not surfaced as `Some("")`.
    #[test]
    fn build_link_metadata_empty_units_omitted() {
        use epics_base_rs::server::snapshot::DisplayInfo;
        let mut snap = Snapshot::new(
            EpicsValue::Double(0.0),
            0,
            0,
            std::time::SystemTime::UNIX_EPOCH,
        );
        snap.display = Some(DisplayInfo {
            units: "".into(),
            precision: 0,
            ..Default::default()
        });
        let md = build_link_metadata(Some(DbFieldType::Double), Some(1), Some(&snap), None);
        assert_eq!(md.units, None);
        assert_eq!(md.precision, Some(0));
    }

    /// every CA native field type maps to its
    /// `LinkDbfType`.
    #[test]
    fn map_dbf_type_covers_every_variant() {
        assert_eq!(map_dbf_type(DbFieldType::String), LinkDbfType::String);
        assert_eq!(map_dbf_type(DbFieldType::Short), LinkDbfType::Short);
        assert_eq!(map_dbf_type(DbFieldType::Float), LinkDbfType::Float);
        assert_eq!(map_dbf_type(DbFieldType::Enum), LinkDbfType::Enum);
        assert_eq!(map_dbf_type(DbFieldType::Char), LinkDbfType::Char);
        assert_eq!(map_dbf_type(DbFieldType::Long), LinkDbfType::Long);
        assert_eq!(map_dbf_type(DbFieldType::Double), LinkDbfType::Double);
        assert_eq!(map_dbf_type(DbFieldType::Int64), LinkDbfType::Int64);
        assert_eq!(map_dbf_type(DbFieldType::UInt64), LinkDbfType::UInt64);
    }

    /// Stage C3 guard, mirroring the PVA connection-scope guard
    /// (`epics-pva-rs/src/server_native/tcp.rs`
    /// `connection_scope_spawns_go_through_the_runtime_seam`): every task
    /// the `calink` production surface spawns must go through
    /// `runtime::task::spawn`, never a bare `tokio::spawn`, a
    /// `tokio::runtime::Handle::spawn`, or a `tokio::runtime::Handle`
    /// field pinned into a production type. A bare `tokio::spawn` panics
    /// on a thread with no tokio runtime — which is exactly the
    /// callback-band worker the RTEMS exec backend runs these tasks on —
    /// and it panics at *runtime*, on the target, not here. So pin it as
    /// source inspection over the whole calink module (`resolver.rs`,
    /// `mod.rs`, `iocsh.rs`).
    ///
    /// Before Stage C3 the CA client passed this guard and calink did
    /// not (design doc §5.1, §6 Stage C3); that asymmetry is what the
    /// guard exists to close and keep closed.
    ///
    /// The needles are assembled with `concat!` so this test's own source
    /// text cannot satisfy the check it performs.
    #[test]
    fn calink_production_spawns_go_through_the_runtime_seam() {
        // Production scope of a calink file ends at its first column-0
        // `#[cfg(test)]` (whole file when there is none — `mod.rs`).
        fn prod(src: &str) -> &str {
            match src.find("\n#[cfg(test)]") {
                Some(i) => &src[..i],
                None => src,
            }
        }

        let resolver = prod(include_str!("resolver.rs"));
        let module = prod(include_str!("mod.rs"));
        let iocsh = prod(include_str!("iocsh.rs"));

        // Fail closed: an earlier `#[cfg(test)]` helper must not shrink a
        // slice past the code this guard is meant to cover.
        assert!(
            resolver.contains("pub async fn open"),
            "resolver production slice no longer covers the link-open path"
        );
        assert!(
            module.contains("calink_link_set_install"),
            "mod production slice no longer covers the link-set installer"
        );
        assert!(
            iocsh.contains("ca_caxr_command"),
            "iocsh production slice no longer covers the caxr command"
        );

        // Positive: calink production actually spawns through the seam.
        assert!(
            resolver.contains(concat!("task", "::spawn(")),
            "resolver production must spawn through `runtime::task::spawn`"
        );

        // Negative: none of the three forbidden spawn shapes appear in any
        // calink production slice.
        let bare_spawn = concat!("tokio", "::spawn(");
        let handle_spawn = concat!("handle", ".spawn(");
        let handle_type = concat!("tokio::runtime", "::Handle");
        for (name, src) in [
            ("resolver.rs", resolver),
            ("mod.rs", module),
            ("iocsh.rs", iocsh),
        ] {
            assert_eq!(
                src.matches(bare_spawn).count(),
                0,
                "{name}: production must spawn through `runtime::task::spawn`; \
                 found bare `{bare_spawn}`"
            );
            assert_eq!(
                src.matches(handle_spawn).count(),
                0,
                "{name}: production must not call `{handle_spawn}` — spawn through the seam"
            );
            assert_eq!(
                src.matches(handle_type).count(),
                0,
                "{name}: production must hold no `{handle_type}` field or call"
            );
        }
    }
}
