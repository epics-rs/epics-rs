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
    connected: Arc<AtomicBool>,
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
        if !self.connected.load(Ordering::Acquire) {
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

    /// Current cached value, or `None` when the link is not servable: no
    /// event yet, the circuit is down, or the cached snapshot's type/count
    /// no longer matches the channel after an upstream type/count change
    /// (C `dbCaGetLink` "not connected" / invalid-cache paths). A
    /// non-servable link serves no value, so a downstream IOC outage or
    /// type change does not leak a stale/mis-shaped value into the owning
    /// record.
    pub fn value(&self) -> Option<EpicsValue> {
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
        if !self.connected.load(Ordering::Acquire) {
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
        let connected = Arc::new(AtomicBool::new(false));
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
    connected: Arc<AtomicBool>,
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
                connected.store(true, Ordering::Release);
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
    // Subscription ended (channel dropped). Reflect the disconnect.
    connected.store(false, Ordering::Release);
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
    connected: Arc<AtomicBool>,
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

/// Apply one connection event to the live-connection `flag`, returning
/// `true` iff this was a `Connected` transition (the caller then kicks
/// off a metadata refetch). `Disconnected` clears the flag — an echo
/// timeout arrives as `Disconnected` too, exactly as `CA_OP_CONN_DOWN`
/// does in C; `AccessRightsChanged`/`NativeTypeChanged` leave it untouched.
/// Factored out of [`run_connection_watcher`] so the flag logic — the
/// disconnect-tracking regression — is unit-testable without a live CA
/// channel.
fn note_conn_event(evt: &crate::client::ConnectionEvent, flag: &AtomicBool) -> bool {
    use crate::client::ConnectionEvent;
    match evt {
        ConnectionEvent::Connected => {
            flag.store(true, Ordering::Release);
            true
        }
        ConnectionEvent::Disconnected => {
            flag.store(false, Ordering::Release);
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
    // Limits/precision/units from a single DBR_CTRL get. Count 1: the
    // limits live in the metadata header, so there is no need to pull a
    // whole waveform just for attributes (C issues the attribute get as a
    // scalar `ca_get_callback(DBR_CTRL_DOUBLE, ...)`).
    let ctrl = match channel.get_with_metadata_count(DbrClass::Ctrl, 1).await {
        Ok(snap) => Some(snap),
        Err(e) => {
            tracing::debug!(
                pv = %pv_name,
                error = %e,
                "calink: CTRL attribute get failed; serving DBF type / element count only"
            );
            None
        }
    };
    meta.store(Arc::new(Some(build_link_metadata(
        dbf,
        element_count,
        ctrl.as_ref(),
    ))));
}

/// Map a fetched CTRL [`Snapshot`] plus the channel's native DBF type /
/// element count into a [`LinkMetadata`]. Pure transform, factored out
/// of [`fetch_link_metadata`] so the field mapping is unit-testable
/// without a live CA server.
///
/// Each `LinkMetadata` field is `None` when the source carried nothing:
/// a String/Enum PV's CTRL reply has no `display`/`control`, so its
/// limits/precision/units stay `None` and the owning record keeps its
/// local defaults — exactly as the C getters leave the caller's buffer
/// untouched on a missing attribute. Alarm-limit order is
/// `(lolo, lo, hi, hihi)`, matching C `getAlarmLimits` (`dbCa.c:758`).
fn build_link_metadata(
    dbf: Option<DbFieldType>,
    element_count: Option<u32>,
    ctrl: Option<&Snapshot>,
) -> LinkMetadata {
    let mut md = LinkMetadata {
        dbf_type: dbf.map(map_dbf_type),
        element_count: element_count.map(|n| n as i64),
        ..LinkMetadata::default()
    };
    if let Some(snap) = ctrl {
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
                md.description = Some(d.description.clone());
            }
        }
        if let Some(c) = snap.control.as_ref() {
            md.control_limits = Some((c.lower_ctrl_limit, c.upper_ctrl_limit));
        }
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

    async fn get_value(&self, name: &str) -> Option<EpicsValue> {
        let name = strip_ca_scheme(name);
        self.link_for(name).await?.value()
    }

    /// C `dbCaGetLink` (`dbCa.c:448-535`): copy out of the buffer the CA
    /// monitor keeps fresh, never open and never wait. Here that buffer is
    /// [`CaLink::value`], refreshed by [`run_monitor`] on every subscription
    /// event (the `eventCallback` analogue, `dbCa.c:925`).
    ///
    /// The difference from [`Self::get_value`] is the missing
    /// [`Self::link_for`] fallback (`resolver.rs:452-457`), which opens the
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
    /// map plus the `CaLink::connected` flag the connection watcher owns
    /// ([`note_conn_event`]). No I/O — the database asks this on the
    /// record-processing thread, inside the record's advisory write gate.
    ///
    /// A link this resolver has never opened reports `Unopened` rather than
    /// `Disconnected`: C opens at `dbCaAddLink` (record init) so the `caLink`
    /// always exists by the first put, while this resolver opens lazily
    /// inside `put_value` / `get_value`. Reporting `Disconnected` would
    /// refuse the very write whose staging performs the open, and the link
    /// would never connect.
    fn put_admission(&self, name: &str) -> PutAdmission {
        let name = strip_ca_scheme(name);
        let links = self.links.read();
        match links.get(name) {
            None => PutAdmission::Unopened,
            Some(link) if link.is_connected() => PutAdmission::Connected,
            Some(_) => PutAdmission::Disconnected,
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
        let connected = AtomicBool::new(false);

        // Circuit comes up — flag true, and signals an attribute refetch.
        assert!(note_conn_event(&ConnectionEvent::Connected, &connected));
        assert!(connected.load(Ordering::Acquire));

        // Upstream IOC restart — circuit drops. Flag MUST go false; no
        // refetch on a disconnect.
        assert!(!note_conn_event(&ConnectionEvent::Disconnected, &connected));
        assert!(!connected.load(Ordering::Acquire));

        // Reconnect — flag true again (CA monitors auto-restore), refetch
        // signalled again.
        assert!(note_conn_event(&ConnectionEvent::Connected, &connected));
        assert!(connected.load(Ordering::Acquire));

        // Echo timeout (TCP up, server hung) reaches the watcher as a plain
        // `Disconnected` — C's `unresponsiveCircuitNotify` fires the same
        // `CA_OP_CONN_DOWN` — so it clears the flag with no refetch.
        assert!(!note_conn_event(&ConnectionEvent::Disconnected, &connected));
        assert!(!connected.load(Ordering::Acquire));

        // An access-rights change is neutral: it leaves the flag as-is
        // and signals no refetch.
        connected.store(true, Ordering::Release);
        assert!(!note_conn_event(
            &ConnectionEvent::AccessRightsChanged {
                read: true,
                write: false,
            },
            &connected,
        ));
        assert!(connected.load(Ordering::Acquire));
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
        let md = build_link_metadata(Some(DbFieldType::Double), Some(1), Some(&snap));
        assert_eq!(md.dbf_type, Some(LinkDbfType::Double));
        assert_eq!(md.element_count, Some(1));
        assert_eq!(md.graphic_limits, Some((-50.0, 100.0)));
        assert_eq!(md.control_limits, Some((-45.0, 95.0)));
        assert_eq!(md.alarm_limits, Some((-40.0, -20.0, 80.0, 90.0)));
        assert_eq!(md.precision, Some(3));
        assert_eq!(md.units.as_deref(), Some("degC"));
    }

    /// a CTRL reply with no `display`/`control` (a
    /// String/Enum PV) yields only the channel-info fields; every limit
    /// stays `None` so the owning record keeps its local default.
    #[test]
    fn build_link_metadata_string_pv_has_no_limits() {
        let snap = Snapshot::new(
            EpicsValue::String("x".into()),
            0,
            0,
            std::time::SystemTime::UNIX_EPOCH,
        );
        let md = build_link_metadata(Some(DbFieldType::String), Some(1), Some(&snap));
        assert_eq!(md.dbf_type, Some(LinkDbfType::String));
        assert_eq!(md.element_count, Some(1));
        assert_eq!(md.graphic_limits, None);
        assert_eq!(md.control_limits, None);
        assert_eq!(md.alarm_limits, None);
        assert_eq!(md.precision, None);
        assert_eq!(md.units, None);
    }

    /// when the channel info fetch failed (`None` dbf /
    /// count) and there is no CTRL reply, every field is `None` — the
    /// link reports "no metadata yet" rather than fabricating zeros.
    #[test]
    fn build_link_metadata_no_info_no_ctrl_is_all_none() {
        let md = build_link_metadata(None, None, None);
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
        let md = build_link_metadata(Some(DbFieldType::Double), Some(1), Some(&snap));
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
