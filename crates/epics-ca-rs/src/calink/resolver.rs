//! [`CaLinkResolver`] — a [`LinkSet`] backend that resolves CA record
//! links through a live [`crate::client::CaClient`].
//!
//! Each record link gets its own [`CaLink`], as each C `caLink` has its own
//! channel: a CA channel, a subscription, and a monitor task that keeps an
//! [`arc_swap::ArcSwap`] snapshot current. The [`LinkSet`] read methods serve from that
//! cache — never a synchronous per-read network fetch. This is the
//! C `dbCa.c` model: `dbCaGetLink` (`dbCa.c:419`) reads the value
//! cached by the monitor `eventCallback` (`dbCa.c:891`).

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use crate::DbFieldType;
use crate::client::{CaChannel, CaClient, ConnFold, EnumReadback};
use arc_swap::{ArcSwap, ArcSwapOption};
use epics_base_rs::error::CaOp;
use epics_base_rs::runtime::task;
use epics_base_rs::server::database::{
    CaLinkInit, CpWorker, EnumString, ExternalCpProcess, LinkDiagnostics, LinkHolder, LinkMetadata,
    LinkPutDone, LinkPutOp, LinkRef, LinkSerial, LinkSet, PutAdmission, PvDatabase,
    link_put_convert, report_put_not_issued,
};
use epics_base_rs::server::snapshot::Snapshot;
use epics_base_rs::types::DBR_CTRL_DOUBLE;
use epics_base_rs::types::{EpicsValue, PvString, RecordName, WallTime};
use parking_lot::RwLock;

use epics_base_rs::server::database::CA_LINK_EVENT_MASK as CALINK_EVENT_MASK;

/// A cached monitor snapshot plus the channel native element count it was
/// produced under.
///
/// The native DBR *type* is intrinsic to `snapshot.value` (recomputed at
/// read time via [`EpicsValue::dbr_type`]), but the native element *count*
/// is not recoverable from a possibly-partial waveform payload, so it is
/// captured here at store time. [`CaLink`]'s read accessors serve this only
/// while both still match the channel's current native description — see
/// [`CaLink::with_servable`] / C `dbCa.c:835-859`.
struct CachedSnapshot {
    snapshot: Snapshot,
    native_count: u32,
}

/// C `pca->stat`, `sevr` and `timeStamp`: every event of either monitor
/// overwrites them (`dbCa.c:1001-1003`), and `getAlarm` / `getTimeStamp`
/// serve the last (`dbCa.c:672-693`).
#[derive(Clone, Copy)]
struct LinkStamp {
    status: u16,
    severity: u16,
    time: WallTime,
}

impl LinkStamp {
    fn of(snapshot: &Snapshot) -> Self {
        Self {
            status: snapshot.alarm.status,
            severity: snapshot.alarm.severity,
            time: snapshot.timestamp,
        }
    }
}

/// C `evidString` with its `pgetString`: the `DBR_STRING` monitor of an
/// ENUM channel, valid only for the native description it was opened
/// under. Flag, subscription and value are one generation, so dropping it is
/// C's whole clear of the three (`dbCa.c:878-903`).
struct StringMonitor {
    native: (DbFieldType, u32),
    value: ArcSwapOption<PvString>,
    task: std::sync::OnceLock<AbortOnDrop>,
}

/// What dbCa's two monitors write: `pgetNative`, the string monitor, and
/// the stamp both overwrite.
struct LinkEvents {
    native: ArcSwap<Option<CachedSnapshot>>,
    /// `Some` is C `pvlOptInpString`.
    string: parking_lot::Mutex<Option<Arc<StringMonitor>>>,
    stamp: ArcSwapOption<LinkStamp>,
}

impl LinkEvents {
    fn new() -> Self {
        Self {
            native: ArcSwap::from_pointee(None),
            string: parking_lot::Mutex::new(None),
            stamp: ArcSwapOption::empty(),
        }
    }

    fn string_wanted(&self) -> bool {
        self.string.lock().is_some()
    }

    /// The string monitor open for `native`, else a new one from `spawn`,
    /// replacing one opened for another description.
    fn open_string(
        &self,
        native: (DbFieldType, u32),
        spawn: impl FnOnce(std::sync::Weak<StringMonitor>) -> AbortOnDrop,
    ) -> Arc<StringMonitor> {
        let mut slot = self.string.lock();
        if let Some(open) = slot.as_ref().filter(|m| m.native == native) {
            return open.clone();
        }
        let monitor = Arc::new(StringMonitor {
            native,
            value: ArcSwapOption::empty(),
            task: std::sync::OnceLock::new(),
        });
        let _ = monitor.task.set(spawn(Arc::downgrade(&monitor)));
        let stale = slot.replace(monitor.clone());
        drop(slot);
        drop(stale);
        monitor
    }

    /// C `connectionCallback`'s size-or-type-change clear, string half
    /// (`dbCa.c:878-903`): a connect under another description drops the
    /// string monitor, and the next string read re-adds it.
    fn drop_stale_string(&self, native: (DbFieldType, u32)) {
        let mut slot = self.string.lock();
        let stale = slot
            .as_ref()
            .is_some_and(|m| m.native != native)
            .then(|| slot.take());
        drop(slot);
        drop(stale);
    }

    /// One `DBR_STRING` event of `monitor`, taken only while it is the open
    /// monitor and the channel still has its description: C clears it in
    /// the connection callback, ahead of the new connection's events.
    fn note_string(
        &self,
        monitor: &Arc<StringMonitor>,
        native: Option<(DbFieldType, u32)>,
        connected: &LinkConnState,
        snapshot: Snapshot,
    ) {
        let slot = self.string.lock();
        if native != Some(monitor.native) || !slot.as_ref().is_some_and(|m| Arc::ptr_eq(m, monitor))
        {
            return;
        }
        let stamp = LinkStamp::of(&snapshot);
        let EpicsValue::String(s) = snapshot.value else {
            return;
        };
        self.stamp.store(Some(Arc::new(stamp)));
        monitor.value.store(Some(Arc::new(s)));
        connected.request_process();
    }
}

/// True iff a cached snapshot remains servable: the value's CA-wire DBR
/// type and the channel native element count it was taken under both still
/// equal the channel's current native description. Mirrors C
/// `dbCa.c:835-859`, which refuses the old cache once a reconnect changes
/// the element count or DBR type, until a matching monitor event
/// repopulates it (`dbCaGetLink` invalid-cache path `dbCa.c:455-464`).
///
/// Pure (no `self`) so the type/count gate is unit-testable without a live
/// CA channel — the same factoring as [`LinkConnState::sync`].
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
}

/// One open CA link — a monitor-backed cache of a remote PV.
///
/// Mirrors C `caLink` (`dbCa.c`): a CA channel plus a subscription
/// whose callback refreshes the cached value. The cache is the only
/// thing the synchronous [`LinkSet`] read path touches. An opaque
/// handle, opened by [`LinkSet::connect_link`] for one record link.
pub struct CaLink {
    /// Latest monitor snapshot plus the native description it was produced
    /// under. `None` until the first event arrives (channel not yet
    /// connected / no value cached) — the C `dbCaGetLink` "not connected"
    /// case. Served only while the cached description still matches the
    /// channel's current native description (see [`Self::with_servable`]).
    events: Arc<LinkEvents>,
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
    /// (`dbCa.c:818-830`) — and a bare `AtomicBool` let three call sites
    /// do the first without the second (stage C6 criterion 4).
    connected: Arc<LinkConnState>,
    /// Cached remote CTRL attributes (display/control/alarm limits,
    /// precision, units) plus the channel's native DBF type and element
    /// count. `None` until the first attribute fetch completes; the
    /// connection-event watcher re-fetches on every (re)connection.
    /// Mirrors C `dbCa.c`: `connectionCallback`
    /// (`dbCa.c:880`) schedules `CA_GET_ATTRIBUTES` on connect, and
    /// `getAttribEventCallback` (`dbCa.c:1042-1091`) caches the
    /// `DBR_CTRL_DOUBLE` reply that `getControlLimits`/`getGraphicLimits`
    /// /`getAlarmLimits`/`getPrecision`/`getUnits` (`dbCa.c:697-772`) later
    /// serve.
    meta: Arc<ArcSwap<Option<LinkMetadata>>>,
    /// The channel — kept alive so the monitor stays subscribed.
    /// Used by the OUT-link write path.
    channel: Arc<CaChannel>,
    /// C `evidNative`: the monitor task, started once the link is a native
    /// input ([`Self::monitor_native`]). Dropping the `CaLink` stops the
    /// task and (via `MonitorHandle::drop`) unsubscribes the remote monitor.
    monitor: std::sync::OnceLock<AbortOnDrop>,
    reactor: task::Reactor,
    pv_name: RecordName,
    /// Abort-on-drop handle for the connection-event watcher task.
    /// Drains `CaChannel::connection_events()` and keeps `connected`
    /// in sync with the real circuit state.
    _conn_task: AbortOnDrop,
    /// The link's one processor of CP/CPP holders ([`run_processor`]), on
    /// the attached database's `dbCaLink` thread; none with no database.
    _process_task: Option<CpWorker>,
}

/// `ca_field_type` / `ca_element_count`; `None` while the channel is not
/// connected.
fn channel_native(channel: &CaChannel) -> Option<(DbFieldType, u32)> {
    match (channel.native_field_type(), channel.element_count()) {
        (Ok(native), Ok(count)) => Some((native, count)),
        _ => None,
    }
}

/// The single owner of a CA link's connection-state transition.
///
/// **Invariant.** A CA link's servability flag MUST NOT go from
/// connected to disconnected without dispatching that PV's CP/CPP holders.
/// C `connectionCallback` (`dbCa.c:817-830`) clears `pca->isConnected` and,
/// in the same critical section, sets `CA_DBPROCESS` for a `pvlOptCP` link
/// (or a `pvlOptCPP` link whose holder is Passive), so the holder processes,
/// its `dbCaGetLink` returns `-1` with `LINK_ALARM`/`INVALID_ALARM`
/// (`dbCa.c:430-435`), and the record lands in LINK/INVALID. Without the
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
/// (`pca->hasReadAccess`, `dbCa.c:831`/`:1027`). [`Self::note_access_rights`]
/// is its only writer, and it performs C `accessRightsCallback`'s two steps
/// (`dbCa.c:1014-1040`) — cache the new rights, then dispatch the CP/CPP
/// holders when a right is lost while connected — as one owned transition,
/// for the same reason the disconnect dispatch lives inside
/// [`Self::mark_disconnected`]: a second site that stored the rights without
/// dispatching would be the §11.4 defect again on the access axis.
struct LinkConnState {
    flag: AtomicBool,
    /// C `pca->hasReadAccess` (`dbCa.c:831`, `:1027`) — the read half of the
    /// last server-granted access rights. Consulted ONLY by the value read
    /// ([`CaLink::value`]), exactly as C consults it only in `dbCaGetLink`
    /// (`dbCa.c:430`); the severity/timestamp/metadata getters (`pcaGetCheck`,
    /// `dbCa.c:621-631`) and the lset `isConnected` (`dbCa.c:604-612`) check
    /// `isConnected` alone.
    ///
    /// `true` at rest, NOT C's calloc-FALSE. C sets `isConnected` and both
    /// access flags inside one `pca->lock` critical section
    /// (`dbCa.c:817-832`), so a connected link never shows the calloc
    /// default. Here `Connected` and the rights that follow it are two
    /// broadcast events, and [`run_monitor`] may open the connected gate
    /// first (a delivered event is proof of liveness), so a `false` default
    /// would refuse the first monitor value of every ordinary full-rights
    /// link until the watcher caught up — a spurious startup LINK alarm C
    /// never shows. The coordinator broadcasts the real rights immediately
    /// after every `Connected`, so the default only lives for that gap.
    read_access: AtomicBool,
    /// C `pca->hasWriteAccess` (`dbCa.c:832`, `:1028`) — the write half of
    /// the same rights event, consulted by `CaResolver::put_admission`
    /// because `dbCaPutLinkCallback`'s gate is
    /// `if (!pca->isConnected || !pca->hasWriteAccess)` (`dbCa.c:529-532`):
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
    /// The database whose CP/CPP holder of the link must be processed on a
    /// disconnect. Attached after the resolver is mounted, hence the lock and
    /// the `Option`; a `None` here is a link opened with no database, which
    /// has no holders to process.
    /// C `pca->nDisconnect` (`dbCa.c:822`), which `dbcar` prints per link
    /// and sums into its trailing `(%lu disconnects, ...)` total
    /// (`dbCaTest.c:99`, `:115`, `:153`). Counted HERE and not at the
    /// watcher, for the reason the CP dispatch is: `mark_disconnected` is
    /// the only true→false edge, so the count cannot double under a
    /// `Disconnected` that arrives twice.
    n_disconnect: AtomicU64,
    /// The connects ([`ConnFold`] outages plus the live one) the watcher has
    /// refetched attributes for. Written only by [`Self::sync`].
    connects_seen: AtomicU64,
    /// C `pvlOptInpNative` (`dbCa.c:456`) — the link has served a native
    /// input transfer. Set by [`CaLink::value`], the port's `dbCaGetLink`.
    ///
    /// C additionally CLEARS this (with its three siblings) when a reconnect
    /// changes the channel's native type or element count
    /// (`dbCa.c:847-849`), and the next `dbCaGetLink` re-sets it; this port
    /// leaves it set, so the two disagree only inside that window.
    used_input_native: AtomicBool,
    /// C `CA_DBPROCESS` in `pca->link_action` (`dbCa.c:155`): requested by
    /// [`Self::request_process`], taken by [`run_processor`]. It owns the
    /// holders' dispatch, so nothing here can process them inline.
    process: ExternalCpProcess,
    db: Arc<RwLock<Option<PvDatabase>>>,
}

impl LinkConnState {
    fn new(db: Arc<RwLock<Option<PvDatabase>>>, link: LinkRef<'_>) -> Self {
        Self {
            flag: AtomicBool::new(false),
            read_access: AtomicBool::new(true),
            write_access: AtomicBool::new(true),
            n_disconnect: AtomicU64::new(0),
            connects_seen: AtomicU64::new(0),
            used_input_native: AtomicBool::new(false),
            process: ExternalCpProcess::new(link),
            db,
        }
    }

    fn is_set(&self) -> bool {
        self.flag.load(Ordering::Acquire)
    }

    /// C `addAction(pca, CA_DBPROCESS)` (`dbCa.c:132-161`): the only way a
    /// CP/CPP holder of this link is processed. A request made while one is
    /// pending merges into it, as the OR into `link_action` does.
    fn request_process(&self) {
        self.process.request();
    }

    /// Mark the link live. Returns `true` on the false→true edge.
    fn mark_connected(&self) -> bool {
        !self.flag.swap(true, Ordering::AcqRel)
    }

    /// Mark the link dead and, on the true→false edge, request the process
    /// of the link's CP/CPP holder, whose link read then fails
    /// unless the link has reconnected first. Returns `true` on the edge.
    fn mark_disconnected(&self) -> bool {
        if !self.flag.swap(false, Ordering::AcqRel) {
            return false;
        }
        // C `connectionCallback` bumps `pca->nDisconnect` in the same
        // critical section that clears `isConnected` and queues the CP
        // dispatch (`dbCa.c:820-826`) — one edge, one count, one owner.
        self.n_disconnect.fetch_add(1, Ordering::Relaxed);
        // `dbCa.c:866-871` asks for this pass; C's `cs_conn` skip at
        // `:1225` is a defect, not copied.
        self.request_process();
        true
    }

    /// The read half of the cached access rights — C `dbCaGetLink`'s
    /// `!pca->hasReadAccess` operand (`dbCa.c:430`).
    fn has_read_access(&self) -> bool {
        self.read_access.load(Ordering::Acquire)
    }

    /// The write half — `dbCaPutLinkCallback`'s `!pca->hasWriteAccess`
    /// operand (`dbCa.c:529`).
    fn has_write_access(&self) -> bool {
        self.write_access.load(Ordering::Acquire)
    }

    /// C `pca->nDisconnect` (`dbCa.c:822`).
    fn disconnect_count(&self) -> u64 {
        self.n_disconnect.load(Ordering::Relaxed)
    }

    /// Record that the link served a native input transfer — C setting
    /// `pvlOptInpNative` on the `dbCaGetLink` that needs the native monitor
    /// (`dbCa.c:455-457`).
    fn note_input_native(&self) {
        self.used_input_native.store(true, Ordering::Relaxed);
    }

    fn used_input_native(&self) -> bool {
        self.used_input_native.load(Ordering::Relaxed)
    }

    /// THE connection transition, run on every watcher wake — an event and
    /// a lag alike: bring the link to `fold`, the state of a receiver that
    /// lost nothing. Every outage the link has not counted is replayed as
    /// the edge it was, dispatch included, so an outage that began and
    /// ended inside a lag still lands LINK/INVALID on the CP holders. A
    /// connect not yet seen takes its rights without a dispatch, C
    /// `connectionCallback` reading `ca_read_access` (`dbCa.c:875-876`); a
    /// later change goes through [`Self::note_access_rights`]. The connect
    /// is counted, not read off `flag`, which a monitor event may set first.
    /// Returns `true` when `fold` holds a connect not yet refetched for.
    fn sync(&self, fold: ConnFold) -> bool {
        for _ in self.disconnect_count()..fold.outages {
            self.mark_connected();
            self.mark_disconnected();
        }
        let connects = fold.outages + u64::from(fold.connected);
        let new_connect = self.connects_seen.swap(connects, Ordering::Relaxed) < connects;
        if fold.connected {
            self.mark_connected();
            if new_connect {
                self.read_access.store(fold.read, Ordering::Release);
                self.write_access.store(fold.write, Ordering::Release);
            } else if (fold.read, fold.write) != (self.has_read_access(), self.has_write_access()) {
                self.note_access_rights(fold.read, fold.write);
            }
        }
        new_connect
    }

    /// C `accessRightsCallback` (`dbCa.c:1014-1040`) as one owned
    /// transition. Returns `true` iff the CP/CPP holders' process was requested.
    ///
    /// * **Not connected:** do nothing at all — C returns before touching
    ///   the cached flags (`dbCa.c:1022-1023`, "connectionCallback will
    ///   handle"). [`Self::sync`] is that connection callback: it takes a
    ///   new connect's rights itself, and skipping here is what keeps a
    ///   stale rights event queued behind a `Disconnected` from
    ///   double-dispatching an outage the disconnect edge already
    ///   dispatched.
    /// * **Connected:** cache both new rights, then dispatch the
    ///   holders UNLESS both read and write are held (`dbCa.c:1029`
    ///   `if (hasReadAccess && hasWriteAccess) goto done`). C processes on
    ///   the loss of EITHER right — not read loss alone — and processes
    ///   NOTHING on a full regain: the holder's alarm clears on the next
    ///   monitor event, not here. The dispatch is per rights change (one
    ///   `accessRightsCallback` per change in C), not edge-deduplicated.
    ///
    /// A dispatched holder whose link lost READ access fails its value
    /// read ([`CaLink::value`] gates on [`Self::has_read_access`]) and
    /// commits LINK/INVALID — `dbCa.c:430-435`. A holder whose link lost
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
        self.request_process();
        true
    }
}

/// Abort the wrapped task when dropped. A bare handle detaches on drop
/// and would leak the monitor task. Typed on the `runtime::task` spawn
/// seam ([`task::TaskHandle`]) so the monitor/watcher tasks route through
/// the same executor on both backends — the tokio reactor on the host, the
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
    /// `dbCaGetLink` readable-cache check (`dbCa.c:419`, `:455-463`):
    ///
    /// 1. the circuit is up (`connected`, driven by
    ///    `CaChannel::connection_events()`);
    /// 2. a monitor snapshot is cached (the C `pca->pgetNative` populated
    ///    case); AND
    /// 3. that snapshot's native description (DBR type + element count)
    ///    still matches the channel's CURRENT native description.
    ///
    /// (3) is the BRIDGE-106 fix: after an upstream reconnect changes the
    /// type or element count (C `dbCa.c:835-859`), the snapshot cached
    /// under the old description stops being servable until a new monitor
    /// event repopulates a matching cache. The check is read-side and
    /// value-intrinsic, so it has no dependence on the ordering between the
    /// connection-event watcher and the monitor task.
    fn with_servable<R>(&self, f: impl FnOnce(&Snapshot) -> R) -> Option<R> {
        if !self.connected.is_set() {
            return None;
        }
        let guard = self.events.native.load();
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
        match channel_native(&self.channel) {
            Some((cur_type, cur_count)) => cache_native_matches(
                cached.snapshot.value.dbr_type(),
                cached.native_count,
                cur_type,
                cur_count,
            ),
            None => false,
        }
    }

    /// C `isConnected` (`dbCa.c:604-612`): `pca->isConnected`, which
    /// `connectionCallback` sets and clears. Whether a value is cached is
    /// [`Self::value`]'s question; an OUT link never caches one.
    pub fn is_connected(&self) -> bool {
        self.connected.is_set()
    }

    /// The cached write right — `dbCaPutLinkCallback`'s second operand
    /// (`dbCa.c:529`). Separate from [`Self::is_connected`] because C tests
    /// them separately and a write-denied link is still connected: it keeps
    /// serving values to `dbCaGetLink`, which does not consult this.
    pub fn has_write_access(&self) -> bool {
        self.connected.has_write_access()
    }

    /// The iocInit wait's all-conditions gate for this link — C
    /// `testInitReady` (`dbCa.c:835-845` at `ef4829829`, epics-base #856
    /// "dbCa: iocInit wait for all conditions" — post-`R7.0.10` and in no
    /// tag, so this is the one dbCa.c citation here that is not
    /// pin-relative): connected, a monitored link with its first event
    /// servable (C's NATIVE wait bit), AND the attribute
    /// fetch complete (the ATTRIB bit; `fetch_link_metadata` stores
    /// `Some` even when the CTRL get failed, exactly the
    /// action-completed edge `getAttribEventCallback` clears the bit
    /// on). C's STRING bit needs no twin: no read can ask for the string
    /// monitor before the connect (`dbCa.c:430-435` fails first), so the
    /// bit is clear when the link connects.
    pub fn init_ready(&self) -> bool {
        self.is_connected()
            && (self.monitor.get().is_none() || self.with_servable(|_| ()).is_some())
            && self.meta.load().is_some()
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
        // !pca->hasReadAccess` (`dbCa.c:430-435`). The read-access half
        // lives HERE and not in `with_servable`, because C consults
        // `hasReadAccess` only for the value read — `pcaGetCheck`
        // (severity/timestamp/DBF getters, `dbCa.c:621-631`) and the lset
        // `isConnected` (`dbCa.c:604-612`) check `isConnected` alone. A
        // read-denied link therefore still reports connected (its circuit
        // is up; writes may proceed) but serves no value, so a dispatched
        // CP holder's link read fails and commits LINK/INVALID exactly as
        // on the disconnect edge.
        if !self.connected.has_read_access() {
            return None;
        }
        self.with_servable(|s| s.value.clone())
    }

    /// Mark that the LINK SET was asked for this link's value — C setting
    /// `pvlOptInpNative` and adding `CA_MONITOR_NATIVE` inside `dbCaGetLink`
    /// (`dbCa.c:455-457`), after the `!isConnected || !hasReadAccess` bail
    /// and ahead of the `!pca->gotInNative` one, so a read of a connected
    /// but still-empty link already monitors it.
    ///
    /// Deliberately NOT inside [`Self::value`]: that is the shared cache
    /// accessor, which `wait_for_link_connected` and other in-crate helpers
    /// also poll. Hooking it there would make the flag mean "somebody looked
    /// at the cache" rather than C's "a link read happened", and every
    /// diagnostic that polls a link would set it.
    fn note_read(&self) {
        if self.connected.is_set() && self.connected.has_read_access() {
            self.monitor_native();
        }
    }

    /// `dbCaGetLink`: the cache as it was, then the mark. C queues
    /// `CA_MONITOR_NATIVE` only after the read's unlock, so the read that
    /// asks for the monitor never sees its first event.
    fn read(&self) -> Option<EpicsValue> {
        let value = self.value();
        self.note_read();
        value
    }

    /// C `pvlOptInpNative` with its `CA_MONITOR_NATIVE` (`dbCa.c:1218-1235`):
    /// subscribe once.
    fn monitor_native(&self) {
        self.connected.note_input_native();
        self.monitor.get_or_init(|| {
            AbortOnDrop(self.reactor.spawn(run_monitor(
                self.channel.clone(),
                self.events.clone(),
                self.connected.clone(),
                self.pv_name.clone(),
            )))
        });
    }

    /// `dbCaGetLink`'s `DBR_ENUM` → `DBR_STRING` branch (`dbCa.c:436-454`),
    /// behind its `!isConnected || !hasReadAccess` bail (`:430-435`): the
    /// `DBR_STRING` monitor's last value, the monitor added by the first
    /// read that asks.
    fn enum_string(&self) -> EnumString {
        let native = match channel_native(&self.channel) {
            Some(native @ (DbFieldType::Enum, _)) => native,
            Some(_) => return EnumString::NotServed,
            None => return EnumString::Pending,
        };
        if !self.connected.is_set() || !self.connected.has_read_access() {
            return EnumString::Pending;
        }
        // C `pvlOptInpString` with its `CA_MONITOR_STRING`
        // (`dbCa.c:1236-1248`).
        let monitor = self.events.open_string(native, |monitor| {
            AbortOnDrop(self.reactor.spawn(run_string_monitor(
                self.channel.clone(),
                self.events.clone(),
                monitor,
                self.connected.clone(),
                self.pv_name.clone(),
            )))
        });
        monitor
            .value
            .load_full()
            .map_or(EnumString::Pending, |s| EnumString::Got((*s).clone()))
    }

    /// This link's `dbcar` report state — C reading the `caLink` fields
    /// `dbCaTest.c:95-123` prints, minus the staged-write count, which in
    /// this port belongs to the database's link-put queue rather than to the
    /// link.
    ///
    /// `connected` is the connection-state owner's flag, C's
    /// `ca_field_type(pca->chid) != TYPENOTCONN` (`dbCaTest.c:97`).
    pub async fn diagnostics(&self) -> LinkDiagnostics {
        LinkDiagnostics {
            connected: self.connected.is_set(),
            // C prints whatever `ca_host_name` returns, and for an
            // unconnected chid libca answers the literal `"<disconnected>"`
            // (`oldChannelNotify.cpp:189-196`).
            host: self
                .channel
                .host_name()
                .await
                .unwrap_or_else(|_| b"<disconnected>".to_vec()),
            read_access: self.connected.has_read_access(),
            write_access: self.connected.has_write_access(),
            n_disconnect: self.connected.disconnect_count(),
            input_native: self.connected.used_input_native(),
            input_string: self.events.string_wanted(),
            // Both OUT bits are dead at the pin: `dbCa.c:541` and `:557` are
            // inside `/* Disabled by ANJ ... */`, so C never sets them and
            // `dbcar` always prints these two columns blank.
            output_native: false,
            output_string: false,
        }
    }

    /// Current cached alarm severity (0..3), or `None` when the link
    /// is not connected. Mirrors C `dbCaGetAlarmLimits` reading the
    /// cached `pca->sevr` — gated on `pca->connected`.
    pub fn alarm_severity(&self) -> Option<i32> {
        self.stamp().map(|s| s.severity as i32)
    }

    /// Cached alarm *status* code (the EPICS `alarm_status` enum), or
    /// `None` when the link is not connected. lets an
    /// `MSS`-modified CA link propagate the remote STAT into the owning
    /// record instead of the generic `LINK_ALARM`. Gated on `connected`
    /// exactly like [`Self::alarm_severity`].
    pub fn alarm_status(&self) -> Option<i32> {
        self.stamp().map(|s| s.status as i32)
    }

    /// Cached timestamp as `(seconds_past_epoch, nanoseconds, userTag)`,
    /// or `None` when the link is not connected. The Channel Access wire
    /// protocol's `DBR_TIME_*` payload carries no user tag, so the tag is
    /// always `0` — only PVA links can adopt a remote `timeStamp.userTag`.
    pub fn time_stamp(&self) -> Option<(i64, i32, u64)> {
        self.stamp().map(|s| {
            let dur = s.time.since_unix_epoch();
            (dur.as_secs() as i64, dur.subsec_nanos() as i32, 0)
        })
    }

    /// The last event's stamp, behind `pcaGetCheck`'s `isConnected` alone
    /// (`dbCa.c:650-660`).
    fn stamp(&self) -> Option<LinkStamp> {
        if !self.connected.is_set() {
            return None;
        }
        self.events.stamp.load().as_deref().copied()
    }

    /// Cached remote metadata (display/control/alarm limits, precision,
    /// units, DBF type, element count), or `None` when the link is not
    /// connected. Gated on `connected` exactly like
    /// [`Self::value`]/[`Self::alarm_severity`] — C `pcaGetCheck`
    /// (`dbCa.c:621`) returns `-1` from every metadata getter while the
    /// CA link is disconnected, so the owning record keeps its local
    /// default rather than adopting stale remote limits.
    pub fn link_metadata(&self) -> Option<LinkMetadata> {
        if !self.connected.is_set() {
            return None;
        }
        self.meta.load().as_ref().clone()
    }
}

/// The network half of the `ca` link set: the database's own `ca` link set
/// serves every name the database does, as C's `dbContext` does, and hands
/// this one the rest.
///
/// Holds a single shared [`CaClient`] and a registry of open [`CaLink`]s,
/// one per record link as C gives each `caLink` its own channel, so two
/// records naming one remote PV are two channels and two subscriptions on
/// the client's one circuit. Cheap to clone — every field is `Arc`-backed.
#[derive(Clone)]
pub struct CaLinkResolver {
    /// Shared CA client, created lazily on the first link open. An IOC with
    /// no CA links never spins one up — the same lazy-client shape as
    /// `pvalink`'s per-link `PvaClient` (the C `dbCa` client is likewise
    /// only created once a link is added). Seeded eagerly by
    /// [`Self::with_client`] when a caller wants to share/pin a client.
    client: Arc<tokio::sync::OnceCell<Arc<CaClient>>>,
    /// Open links by [`LinkSerial`], each with its PV name.
    links: Arc<RwLock<HashMap<LinkSerial, (RecordName, Arc<CaLink>)>>>,
    /// Database handle the link's processor uses to process external
    /// CP/CPP holder records on each remote change
    /// ([`ExternalCpProcess::run_worker`]). `None` until
    /// [`Self::attach_database`] is called at IOC assembly. Late-bound
    /// behind a lock so the cheaply-`Clone`d resolver can have the DB
    /// attached after construction — the same shape as `pvalink`'s
    /// `PvaLinkResolver::db`.
    db: Arc<RwLock<Option<PvDatabase>>>,
    /// Where every link task is spawned. Handed in by the owner rather
    /// than taken from the client, which is created lazily.
    reactor: task::Reactor,
}

impl CaLinkResolver {
    /// Build a resolver whose shared [`CaClient`] is created lazily on
    /// the first link open. Infallible — an IOC with no such link never constructs a
    /// client, and any client-init failure surfaces at that open
    /// (mirroring `pvalink`'s lazy client), so installation cannot fail
    /// and never aborts the IOC.
    pub fn new(reactor: task::Reactor) -> Self {
        Self {
            client: Arc::new(tokio::sync::OnceCell::new()),
            links: Arc::new(RwLock::new(HashMap::new())),
            db: Arc::new(RwLock::new(None)),
            reactor,
        }
    }

    /// Build a resolver around an already-constructed [`CaClient`].
    /// Lets a caller share one client across the CA gateway and the
    /// CA links, or pin the client to a specific server in tests. The
    /// client is seeded eagerly, so the lazy init is bypassed.
    pub fn with_client(client: Arc<CaClient>) -> Self {
        Self {
            reactor: client.reactor().clone(),
            client: Arc::new(tokio::sync::OnceCell::new_with(Some(client))),
            links: Arc::new(RwLock::new(HashMap::new())),
            db: Arc::new(RwLock::new(None)),
        }
    }

    /// Get the shared [`CaClient`], creating it on first call. The
    /// `OnceCell` guarantees exactly one client even under concurrent
    /// first opens. A client-init failure is returned (not cached), so a
    /// later open retries — matching C `dbCa`'s deferred channel setup.
    pub(super) async fn client(&self) -> Result<&Arc<CaClient>, CaLinkError> {
        self.client
            .get_or_try_init(|| async {
                CaClient::new()
                    .await
                    .map(Arc::new)
                    .map_err(|e| CaLinkError::ClientInit(e.to_string()))
            })
            .await
    }

    /// Attach the database handle the link's processor uses to process
    /// external CP/CPP holder records on each remote change
    /// ([`ExternalCpProcess::run_worker`]). Called by
    /// [`install_calink_resolver`] at IOC assembly — before iocInit's
    /// `setup_cp_links` warms any external CP link — so the handle is
    /// always present by the time the first monitor event fires. The
    /// cross-IOC twin of `pvalink`'s `PvaLinkResolver::attach_database`.
    pub fn attach_database(&self, db: PvDatabase) {
        *self.db.write() = Some(db);
    }

    /// `link`'s [`CaLink`], opened here if it has none: C `dbCaTask`'s
    /// `CA_CONNECT` (`dbCa.c:1140-1159`). A channel, a monitor, and the tasks
    /// that keep its cache current. `None` when the open failed, or the
    /// link was removed while it ran — `CA_CLEAR_CHANNEL` comes first
    /// (`dbCa.c:1200`).
    async fn open_link(&self, link: LinkRef<'_>) -> Option<Arc<CaLink>> {
        if let Some(existing) = self.cached_link(link) {
            return Some(existing);
        }
        let client = self.client().await.ok()?;
        // The link starts as its entry is published, under this lock: two
        // first opens of one link start one channel, and an open the link's
        // removal overtook starts none.
        let mut links = self.links.write();
        if link.is_removed() {
            return None;
        }
        let entry = match links.entry(link.serial()) {
            std::collections::hash_map::Entry::Occupied(won) => return Some(won.get().1.clone()),
            std::collections::hash_map::Entry::Vacant(entry) => entry,
        };
        let pv_name = RecordName::from(link.name);
        let opened = self.start_net(client, pv_name.clone(), link);
        Some(entry.insert((pv_name, opened)).1.clone())
    }

    /// The link over a network channel. MUST NOT block: it runs under the
    /// `links` lock.
    fn start_net(&self, client: &CaClient, pv_name: RecordName, link: LinkRef<'_>) -> Arc<CaLink> {
        // The receiver is taken with the channel, before its search starts:
        // an `AccessRightsChanged` / `Connected` pair published ahead of the
        // watcher is lost for good — `meta` stays empty and the write right
        // stays at its default `true` (C `connectionCallback` scheduling
        // `CA_GET_ATTRIBUTES` and reading `ca_write_access`).
        let (channel, conn_rx) = client.create_channel_with_events(&pv_name, 0);
        let channel = Arc::new(channel);
        let events = Arc::new(LinkEvents::new());
        let connected = Arc::new(LinkConnState::new(self.db.clone(), link));
        let meta: Arc<ArcSwap<Option<LinkMetadata>>> = Arc::new(ArcSwap::from_pointee(None));
        // Connection-event watcher: keeps `connected` in sync with the
        // real circuit state so `is_connected()` reflects upstream
        // disconnects (mirrors `pvalink`'s `monitor_connected` flag), and
        // re-fetches the remote CTRL attributes into `meta` on each
        // connect.
        let reactor = &self.reactor;
        let conn_task = reactor.spawn(run_connection_watcher(
            reactor.clone(),
            conn_rx,
            connected.clone(),
            channel.clone(),
            events.clone(),
            meta.clone(),
            pv_name.clone(),
        ));
        let process_task = self
            .db
            .read()
            .as_ref()
            .map(|db| db.spawn_cp_worker(run_processor(connected.clone())));
        let opened = Arc::new(CaLink {
            events,
            connected,
            meta,
            channel,
            monitor: std::sync::OnceLock::new(),
            reactor: reactor.clone(),
            pv_name,
            _conn_task: AbortOnDrop(conn_task),
            _process_task: process_task,
        });
        // `dbInitLink`'s `pvlOptInpNative` for a `DBF_INLINK`: monitored
        // from the connect (`dbCa.c:865-867`).
        if link.is_input() {
            opened.monitor_native();
        }
        opened
    }

    /// Wait until the record links to `pv_name` have opened and each is
    /// connected, a monitored one with its first event cached. Returns
    /// `false` on timeout. The canonical test / IOC-init helper for
    /// "wait for the upstream IOC to come online".
    pub async fn wait_for_link_connected(
        &self,
        pv_name: impl AsRef<[u8]>,
        timeout: std::time::Duration,
    ) -> bool {
        let name = pv_name.as_ref();
        let deadline = epics_base_rs::runtime::time::deadline_from_now(timeout);
        loop {
            let ready = {
                let links = self.links.read();
                let mut named = links.values().filter(|(n, _)| n.as_bytes() == name);
                let first = named.next();
                first.is_some()
                    && first.into_iter().chain(named).all(|(_, link)| {
                        link.is_connected()
                            && (link.monitor.get().is_none() || link.value().is_some())
                    })
            };
            if ready {
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

    /// The link's served value as `dbcaxr` reports it: no open, and none of
    /// `dbCaGetLink`'s effects on the link.
    pub(super) fn peek_value(&self, link: LinkRef<'_>) -> Option<EpicsValue> {
        self.cached_link(link)?.value()
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
            .values()
            .map(|(name, link)| (name.to_string(), link.is_connected()))
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

    /// The registry read with NO open — the record-processing half of
    /// [`Self::open_link`]. C `dbCaGetLink` and the `getAttributes` family
    /// read `pca->...` under `pca->lock` and never create a channel
    /// (`dbCa.c:419-506`, `:633-675`); the open is `dbCaAddLink`'s
    /// `CA_CONNECT` on the `dbCaTask`, reached here through
    /// [`LinkSet::connect_link`]. Every synchronous `LinkSet` accessor goes
    /// through this, so none of them can suspend the record thread.
    fn cached_link(&self, link: LinkRef<'_>) -> Option<Arc<CaLink>> {
        self.links
            .read()
            .get(&link.serial())
            .map(|(_, l)| l.clone())
    }
}

/// C `dbCaTask`'s `CA_DBPROCESS` arm (`dbCa.c:1185-1195`, `:1314-1321`)
/// for one link: take the pending request, then process the link's CP/CPP
/// holder. The flag is cleared before processing, so a
/// request raised during it processes again, as C's re-queue does.
async fn run_processor(state: Arc<LinkConnState>) {
    // A clone per process: no read guard is held across record processing.
    state.process.run_worker(|| state.db.read().clone()).await;
}

/// One good monitor event: C `dbCa.c eventCallback` refreshes the cached
/// value, then adds `CA_DBPROCESS` (`dbCa.c:891`, `:958-962`). The cache is
/// stored first, so the holder's INP read sees the fresh value.
fn note_monitor_value(
    events: &LinkEvents,
    connected: &LinkConnState,
    snapshot: Snapshot,
    native_count: u32,
) {
    // A delivered monitor event is itself proof of liveness — mark the link
    // connected even if the `Connected` lifecycle event has not been observed
    // yet (race-free, mirrors `pvalink`'s callback).
    connected.mark_connected();
    // A native ENUM event scans only while no string monitor exists
    // (`dbCa.c:932-933`).
    let scan = !(matches!(snapshot.value, EpicsValue::Enum(_)) && events.string_wanted());
    // The stamp first, so a read that sees this value sees its stamp.
    events.stamp.store(Some(Arc::new(LinkStamp::of(&snapshot))));
    events.native.store(Arc::new(Some(CachedSnapshot {
        snapshot,
        native_count,
    })));
    if scan {
        connected.request_process();
    }
}

/// Monitor task: subscribe, then drain the subscription, refresh the cache
/// on every event. Ends when the channel is dropped (`recv` returns `None`).
///
/// Mirrors C `dbCa.c` `eventCallback` (`dbCa.c:891`) — every CA
/// monitor event overwrites the cached value/severity/timestamp that
/// `dbCaGetLink` later serves.
async fn run_monitor(
    channel: Arc<CaChannel>,
    events: Arc<LinkEvents>,
    connected: Arc<LinkConnState>,
    pv_name: RecordName,
) {
    // C `dbCa` opens the record-link monitor with `ca_add_array_event`
    // (`dbCa.c:1225-1229`), whose libca macro expands to
    // `ca_add_masked_array_event(..., DBE_VALUE | DBE_ALARM)`
    // (`cadef.h:2004-2012`). DBE_LOG / DBE_ARCHIVE is a separate
    // event-trigger class (`cadef.h:1148-1158`) that `dbCa` never
    // requests, so a CP/CPP record link must not refresh its cache (or
    // wake a scan) on archive/log-only posts. The default
    // `CaChannel::subscribe()` requests `DBE_VALUE | DBE_LOG |
    // DBE_ALARM`, so request the dbCa mask explicitly here.
    let mut monitor = match channel.subscribe_with_mask(0.0, CALINK_EVENT_MASK).await {
        Ok(monitor) => monitor,
        // `dbCaTask`'s `ca_add_array_event` failure (`dbCa.c:1230-1233`):
        // the link stays without a value.
        Err(e) => {
            tracing::debug!(pv = %pv_name, error = %e, "calink: subscribe failed");
            return;
        }
    };
    while let Some(event) = monitor.recv().await {
        match event {
            Ok(snapshot) => {
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
                note_monitor_value(&events, &connected, snapshot, native_count);
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

/// C `dbCaTask`'s `CA_MONITOR_STRING` (`dbCa.c:1236-1248`): one element of
/// `DBR_TIME_STRING`, each event stored as `pgetString` and processing the
/// holder (`dbCa.c:927-929`).
async fn run_string_monitor(
    channel: Arc<CaChannel>,
    events: Arc<LinkEvents>,
    this: std::sync::Weak<StringMonitor>,
    connected: Arc<LinkConnState>,
    pv_name: RecordName,
) {
    let mut monitor = match channel
        .subscribe_with_mask_readback_count(
            0.0,
            CALINK_EVENT_MASK,
            EnumReadback::Label,
            false,
            Some(1),
        )
        .await
    {
        Ok(monitor) => monitor,
        Err(e) => {
            tracing::debug!(pv = %pv_name, error = %e, "calink: string subscribe failed");
            return;
        }
    };
    while let Some(event) = monitor.recv().await {
        match event {
            Ok(snapshot) => {
                let Some(this) = this.upgrade() else { return };
                events.note_string(&this, channel_native(&channel), &connected, snapshot);
            }
            Err(e) => {
                tracing::debug!(pv = %pv_name, error = %e, "calink: string monitor error ignored");
            }
        }
    }
}

/// Connection-event watcher: keep `connected` in sync with the CA
/// circuit state. `Connected` flips it `true`; `Disconnected` /
/// `Unresponsive` flip it `false` so a downstream IOC restart is
/// reflected by `CaLink::is_connected()`. Mirrors `dbCa.c`'s
/// `connectionCallback` setting `pca->connected`.
///
/// on every `Connected` the watcher also (re)fetches the
/// remote CTRL attributes into `meta`, mirroring `connectionCallback`
/// scheduling `CA_GET_ATTRIBUTES` (`dbCa.c:880`). The fetch is detached
/// so a slow or hung CTRL get never delays the watcher from observing a
/// later disconnect — the metadata is best-effort and the read path
/// gates on `connected` regardless.
async fn run_connection_watcher(
    reactor: epics_base_rs::runtime::task::Reactor,
    mut conn_rx: epics_base_rs::runtime::sync::broadcast::Receiver<crate::client::ConnectionEvent>,
    connected: Arc<LinkConnState>,
    channel: Arc<CaChannel>,
    events: Arc<LinkEvents>,
    meta: Arc<ArcSwap<Option<LinkMetadata>>>,
    pv_name: RecordName,
) {
    loop {
        let step = watch_step(&mut conn_rx, &connected, || channel.conn_fold()).await;
        match step {
            WatchStep::Closed => return,
            WatchStep::Applied => {}
            // Detached so a hung CTRL get never stalls the watcher from
            // seeing a later disconnect.
            WatchStep::Refetch => {
                if let Some(native) = channel_native(&channel) {
                    events.drop_stale_string(native);
                }
                reactor.spawn(fetch_link_metadata(
                    channel.clone(),
                    meta.clone(),
                    pv_name.clone(),
                ));
            }
        }
    }
}

/// What one [`watch_step`] left for the watcher to do.
#[derive(Debug, PartialEq, Eq)]
enum WatchStep {
    Applied,
    /// The link is (or may have become) newly connected: refetch the CTRL
    /// attributes, C `connectionCallback` scheduling `CA_GET_ATTRIBUTES`.
    Refetch,
    /// The channel is gone; the watcher's job is done.
    Closed,
}

/// Wait for one connection event, then [`LinkConnState::sync`] to `fold`.
/// The event is only the wake: a lag wakes the same way, and the fold holds
/// what the dropped events would have done.
async fn watch_step(
    conn_rx: &mut epics_base_rs::runtime::sync::broadcast::Receiver<crate::client::ConnectionEvent>,
    state: &LinkConnState,
    fold: impl Fn() -> ConnFold,
) -> WatchStep {
    use epics_base_rs::runtime::sync::broadcast::error::RecvError;
    match conn_rx.recv().await {
        Ok(_) | Err(RecvError::Lagged(_)) if state.sync(fold()) => WatchStep::Refetch,
        Ok(_) | Err(RecvError::Lagged(_)) => WatchStep::Applied,
        Err(RecvError::Closed) => WatchStep::Closed,
    }
}

/// One-shot CTRL attribute fetch for a CA link, mirroring C `dbCa.c`'s
/// `CA_GET_ATTRIBUTES` → `getAttribEventCallback` (`dbCa.c:1209-1217`,
/// `:1042-1091`): a `DBR_CTRL` get whose reply fills the cached
/// control/display/alarm limits, precision and units. The channel's
/// native DBF type and element count come from the channel info (C
/// `getDBFtype`/`getElements` read `pca->dbrType`/`nelements`, not the
/// CTRL reply). Best-effort: on a failed CTRL get the type/count are
/// still stored and the next reconnect retries the limits.
async fn fetch_link_metadata(
    channel: Arc<CaChannel>,
    meta: Arc<ArcSwap<Option<LinkMetadata>>>,
    pv_name: RecordName,
) {
    // DBF type + element count from the channel's native description.
    let (dbf, element_count) = match channel.info().await {
        Ok(info) => (Some(info.native_type), Some(info.element_count)),
        Err(_) => (None, None),
    };
    // Limits/precision/units from a single DBR_CTRL get, at a FIXED
    // `DBR_CTRL_DOUBLE` and gated on the native type — both straight from C.
    // `dbCa.c:878-880` asks for attributes for every channel whose
    // `pca->dbrType` is not `DBR_STRING`, and `:1210` issues that get as
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
        match async { channel.get_callback(DBR_CTRL_DOUBLE, 1)?.await }.await {
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
    meta.store(Arc::new(Some(LinkMetadata::from_ca_attributes(
        dbf,
        element_count,
        attrs.as_ref(),
    ))));
}

#[async_trait::async_trait]
impl LinkSet for CaLinkResolver {
    fn ca_client_service(
        &self,
    ) -> Option<Arc<dyn epics_base_rs::server::database::CaClientService>> {
        Some(Arc::new(self.clone()))
    }

    fn is_connected(&self, link: LinkRef<'_>) -> bool {
        // Not opened yet — not connected; `connect_link` opens it.
        self.cached_link(link).is_some_and(|l| l.is_connected())
    }

    fn init_ready(&self, link: LinkRef<'_>) -> bool {
        self.cached_link(link).is_some_and(|l| l.init_ready())
    }

    async fn get_value(&self, link: LinkRef<'_>) -> Option<EpicsValue> {
        let link = self.open_link(link).await?;
        link.read()
    }

    /// C `dbCaGetLink` (`dbCa.c:419-506`): copy out of the buffer the CA
    /// monitor keeps fresh, never open and never wait. Here that buffer is
    /// [`CaLink::value`], refreshed by `run_monitor` on every subscription
    /// event (the `eventCallback` analogue, `dbCa.c:891`).
    ///
    /// The difference from [`Self::get_value`] is the missing
    /// `CaLinkResolver::open_link` fallback, which opens the channel and
    /// awaits the subscription round trip. That open happens on the
    /// database's link work owner via [`Self::connect_link`].
    fn get_cached_value(&self, link: LinkRef<'_>) -> Option<EpicsValue> {
        let link = self.cached_link(link)?;
        link.read()
    }

    fn get_cached_enum_string(&self, link: LinkRef<'_>) -> EnumString {
        self.cached_link(link)
            .map_or(EnumString::Pending, |l| l.enum_string())
    }

    /// C `dbCaAddLink`'s `CA_CONNECT` work (`dbCa.c:1140-1159`): create the
    /// channel, and only that — the branch ends `continue; /*Other options
    /// must wait until connect*/`, so the monitor is NOT added here. C adds
    /// it on a later `dbCaTask` pass under `CA_MONITOR_NATIVE`
    /// (`dbCa.c:1218-1235`), which `connectionCallback` schedules at
    /// `dbCa.c:866` once the channel is up. This port does both in one
    /// call; it runs on the link work owner, so the `subscribe` round trip
    /// inside the open is off the record thread either way.
    ///
    /// `init` rides on the link's first event's process, and is released
    /// here when no link opens.
    async fn connect_link(&self, link: LinkRef<'_>, init: Option<CaLinkInit>) {
        if let Some(link) = self.open_link(link).await
            && let Some(init) = init
        {
            link.connected.process.adopt_init(init);
        }
    }

    /// A link not opened yet starts after this, from the `links` lock
    /// `Self::open_link` starts it under.
    fn join_cp_link(&self, link: LinkRef<'_>) {
        if let Some(link) = self.cached_link(link) {
            link.connected.process.join();
        }
    }

    /// `dbCaRemoveLink` (`dbCa.c:431-446`): the link's channel is cleared
    /// with its monitor. A put in flight keeps the channel until it answers.
    fn remove_link(&self, _holder: LinkHolder<'_>, link: LinkRef<'_>) {
        let removed = self.links.write().remove(&link.serial());
        drop(removed);
    }

    /// C `dbCaPutLinkCallback`'s `if (!pca->isConnected || !pca->hasWriteAccess)
    /// return -1;` (`dbCa.c:529-532`), answered from cached state: the links
    /// map plus the `CaLink::connected` owner, which carries both the
    /// connection flag (`sync`) and the cached rights
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
    /// A link this resolver has not opened is refused too: C opens every CA
    /// link at `dbCaAddLink` (iocInit's open pass, or a relink here) and
    /// refuses its puts until it connects.
    fn put_admission(&self, link: LinkRef<'_>) -> PutAdmission {
        match self.cached_link(link) {
            Some(link) if link.is_connected() && link.has_write_access() => PutAdmission::Connected,
            _ => PutAdmission::Refused,
        }
    }

    async fn put_value(
        &self,
        link: LinkRef<'_>,
        value: EpicsValue,
        op: LinkPutOp,
    ) -> Result<(), String> {
        self.put_value_done(link, None, value, op).await.1
    }

    fn put_native(&self, link: LinkRef<'_>) -> Option<(DbFieldType, u32)> {
        channel_native(&self.cached_link(link)?.channel)
    }

    /// Issuing the put and completing it are separate steps, as in C
    /// `dbCaTask`: a put that cannot be issued (channel down, or the
    /// request refused) is [`LinkPutDone::NotIssued`], which the database
    /// drops (`dbCa.c:1225-1247`); an issued put callback completes
    /// when the server answers or the circuit drops, with no put timeout.
    async fn put_value_done(
        &self,
        link: LinkRef<'_>,
        holder: Option<&[u8]>,
        value: EpicsValue,
        op: LinkPutOp,
    ) -> (LinkPutDone, Result<(), String>) {
        // Honour the C dbCore split between a plain link put and a
        // put-notify-aware put. `dbCaPutLink` (no callback) sets
        // `putType = CA_PUT`, and the CA task later issues a
        // fire-and-forget `ca_array_put` — the source record's
        // processing does NOT block on the remote put completing
        // (dbCa.c:598-602 `dbCaPutLink`, dbCa.c:1163-1166 the
        // `CA_PUT` dispatch). Only `dbCaPutLinkCallback`
        // (`putType = CA_PUT_CALLBACK`) issues `ca_array_put_callback`
        // and parks the originating record until completion. The
        // database maps a plain OUT write (put-notify chain included) to
        // `LinkPutOp::Plain` and a `dbPutLinkAsync` / sseq WAITn write
        // to `LinkPutOp::Async`, so the resolver must preserve
        // the split: routing `Plain` through the fire-and-forget
        // `CA_PROTO_WRITE` (`put_nowait`) keeps a slow or hung remote
        // CA server from stalling normal record processing, while `Async`
        // keeps the WRITE_NOTIFY completion the writer waits on.
        let name = link.name;
        let Some(link) = self.open_link(link).await else {
            let reason = format!("CA link {} not open", String::from_utf8_lossy(name));
            return (LinkPutDone::NotIssued, Err(reason));
        };
        // The clamp C applies before the CA request is ever built:
        // `dbCaPutLinkCallback` converts the caller's buffer to the
        // channel's native type and bounds the request to the channel's
        // element count in the same step — `if(nRequest>pca->nelements)
        // nRequest = pca->nelements;` then `aConvert(..., nRequest,
        // pca->nelements, 0)` (`dbCa.c:575-577`), against
        // `pca->nelements = ca_element_count(chid)` (`:862`). The surplus
        // elements are DROPPED and the put succeeds; the same rule holds
        // for a DB target (`dbAccess.c:1360-1361`), so an oversized array put
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
        //
        // No native description: the channel is down, which `dbCaTask`
        // skips without sending (`dbCa.c:1221`).
        let Some((native, nelements)) = channel_native(&link.channel) else {
            let reason = format!("CA link {} not connected", String::from_utf8_lossy(name));
            return (LinkPutDone::NotIssued, Err(reason));
        };
        let value = match link_put_convert(name, value, native, nelements) {
            Ok(value) => value,
            Err(e) => return (LinkPutDone::Callback, Err(e)),
        };
        // Sent under the converted value's own type: the native one, or
        // DBR_STRING for a string put to an enum channel.
        let dbr_type = value.dbr_type() as u16;
        let issued = match op {
            LinkPutOp::Plain => link
                .channel
                .put_as_dbr_nowait(dbr_type, &value)
                .await
                .map(|()| None),
            LinkPutOp::Async => link.channel.put_as_dbr_callback(dbr_type, &value).map(Some),
        };
        match issued {
            Err(e) => {
                report_put_not_issued(e.to_eca_status(CaOp::Write), holder, name);
                (LinkPutDone::NotIssued, Err(e.to_string()))
            }
            Ok(None) => (LinkPutDone::Callback, Ok(())),
            Ok(Some(done)) => (LinkPutDone::Callback, done.await.map_err(|e| e.to_string())),
        }
    }

    fn alarm_severity(&self, link: LinkRef<'_>) -> Option<i32> {
        let sev = self.cached_link(link)?.alarm_severity()?;
        // Mirror the lset contract: only a non-zero severity is a
        // contribution worth propagating into the owning record's
        // LINK_ALARM. `0` (NO_ALARM) means "do not propagate".
        if sev > 0 { Some(sev) } else { None }
    }

    fn alarm_status(&self, link: LinkRef<'_>) -> Option<i32> {
        // surface the remote STAT for `MSS` propagation.
        // Record processing only consults this when the alarm is
        // actually propagated (severity > 0 via `alarm_severity`), so
        // no severity gate is needed here.
        self.cached_link(link)?.alarm_status()
    }

    fn time_stamp(&self, link: LinkRef<'_>) -> Option<(i64, i32, u64)> {
        self.cached_link(link)?.time_stamp()
    }

    fn link_metadata(&self, link: LinkRef<'_>) -> Option<LinkMetadata> {
        // surface the remote display/control/alarm limits,
        // precision, units, DBF type and element count through the DB
        // link API so a record with a CA INP link inherits them, matching
        // the pvalink metadata path. Reads the cached CTRL attributes —
        // no fresh GET — exactly as C `getControlLimits` &c. read
        // `pca->controlLimits`.
        self.cached_link(link)?.link_metadata()
    }

    fn link_names(&self) -> Vec<RecordName> {
        let mut names: Vec<RecordName> =
            self.links.read().values().map(|(n, _)| n.clone()).collect();
        names.sort();
        names.dedup();
        names
    }

    /// `None` for a link this resolver has never opened — C's `pca == NULL`,
    /// which `dbcar` renders as a not-connected link with zero counters
    /// (`dbCaTest.c:127-132`). Never opens the link itself: `dbcar` is a
    /// report, and C's walk reads `plink->value.pv_link.pvt` without
    /// touching CA.
    async fn link_diagnostics(&self, link: LinkRef<'_>) -> Option<LinkDiagnostics> {
        let link = self.cached_link(link)?;
        Some(link.diagnostics().await)
    }
}

/// Install a [`CaLinkResolver`] on `db`, registered under the `"ca"`
/// lset scheme. After this, a record whose link field resolves to
/// `ParsedLink::Ca` (a non-local PV name, or one with a ` CA` modifier)
/// reads through the monitor-backed CA cache via
/// `PvDatabase::resolve_external_pv`.
///
/// Returns the resolver so the caller can pre-open links
/// (`CaLinkResolver::open`) at IOC init and query stats.
///
/// Infallible: the shared CA client is created lazily on the first link
/// open ([`CaLinkResolver::new`]), so installation never spins one up
/// and never fails — a database with no CA links is fully serviceable
/// and an IOC must not be aborted by CA-client init. This is the
/// CA-side twin of the bridge's infallible `install_pvalink_resolver`.
pub async fn install_calink_resolver(db: &PvDatabase, reactor: task::Reactor) -> CaLinkResolver {
    let resolver = CaLinkResolver::new(reactor);
    // Attach the DB before registering the lset, so the monitor callback
    // can request the holders' process from the first event on.
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
    use source_guard::{Comments, production};

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

    fn fold(connected: bool, (read, write): (bool, bool), outages: u64) -> ConnFold {
        ConnFold {
            connected,
            read,
            write,
            outages,
        }
    }

    const RW: (bool, bool) = (true, true);

    /// C `pca->nDisconnect` counts EDGES, not events: `connectionCallback`
    /// bumps it inside `if (!pca->isConnected)` after the flag was
    /// recomputed (`dbCa.c:817-822`), so a repeated disconnect — the
    /// watcher's, then the subscription-ended tail's — adds nothing, and a
    /// reconnect followed by a second drop adds one more.
    #[test]
    fn the_disconnect_count_moves_once_per_edge() {
        let connected = cp_link_state();
        // Never connected: C's `caLink` at its calloc zero.
        connected.sync(fold(false, RW, 0));
        assert_eq!(connected.disconnect_count(), 0);

        connected.sync(fold(true, RW, 0));
        connected.sync(fold(false, RW, 1));
        assert_eq!(connected.disconnect_count(), 1);
        // The subscription-ended tail, then a wake on the same fold.
        connected.mark_disconnected();
        connected.sync(fold(false, RW, 1));
        assert_eq!(connected.disconnect_count(), 1, "one edge, one count");

        connected.sync(fold(true, RW, 1));
        connected.sync(fold(false, RW, 2));
        assert_eq!(connected.disconnect_count(), 2);
    }

    /// BUG 1 regression: a disconnect MUST clear the flag — pre-fix
    /// `is_connected()` keyed off cache presence alone and stayed `true`
    /// once any event had been cached. `sync` returns `true` for each
    /// connect it has not refetched attributes for, the C
    /// `connectionCallback` scheduling `CA_GET_ATTRIBUTES` per connect, and
    /// that includes a connect `run_monitor` flagged first.
    #[test]
    fn bug1_connection_event_tracks_disconnect() {
        let connected = cp_link_state();
        assert!(connected.sync(fold(true, RW, 0)));
        assert!(connected.is_set());
        assert!(
            !connected.sync(fold(true, RW, 0)),
            "one refetch per connect"
        );

        assert!(!connected.sync(fold(false, RW, 1)));
        assert!(!connected.is_set());
        assert!(connected.sync(fold(true, RW, 1)));
        assert!(connected.is_set());

        // A rights change leaves the flag set and closes the read gate.
        assert!(!connected.sync(fold(true, (false, true), 1)));
        assert!(connected.is_set());
        assert!(!connected.has_read_access());

        let early = cp_link_state();
        early.mark_connected();
        assert!(
            early.sync(fold(true, RW, 0)),
            "a monitor event that set the flag first does not eat the refetch"
        );
    }

    /// §11.7 item 1: C `accessRightsCallback` (`dbCa.c:1014-1040`) as the
    /// owner decides it, per invariant boundary — not per narrative:
    ///
    /// * rights event while DISCONNECTED → no dispatch AND no flag update
    ///   (`dbCa.c:1022-1023` returns before touching the cache;
    ///   "connectionCallback will handle"), so a stale rights event queued
    ///   behind a `Disconnected` cannot double-dispatch the outage;
    /// * read lost while connected → dispatch, read gate closes;
    /// * write lost ALONE while connected → dispatch too: the C gate is
    ///   `if (hasReadAccess && hasWriteAccess) goto done` (`dbCa.c:1029`),
    ///   i.e. C processes on the loss of EITHER right, not read alone —
    ///   but the read gate stays open, so the dispatched holder reads a
    ///   good value and lands no alarm;
    /// * full regain → NO dispatch (`dbCa.c:1029`): the holder's alarm
    ///   clears on the next monitor event, not on the rights edge.
    #[test]
    fn access_rights_transitions_follow_dbca() {
        let state = cp_link_state();

        // Disconnected: C's early return. No dispatch, no cache update.
        assert!(!state.note_access_rights(false, false));
        assert!(
            state.has_read_access(),
            "rights must not change while disconnected — `sync` takes the \
             reconnect's rights"
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

        // Write lost alone: dispatch (dbCa.c:1029), read gate stays open.
        assert!(state.note_access_rights(true, false));
        assert!(state.has_read_access());

        // The disconnect edge owns its own dispatch; a rights event
        // arriving after it is the disconnected early-return again — one
        // outage, one dispatch.
        assert!(state.mark_disconnected());
        assert!(!state.note_access_rights(false, false));
    }

    fn block<F: std::future::Future>(f: F) -> F::Output {
        epics_base_rs::runtime::task::block_on_sync(f).expect("a plain test thread")
    }

    /// The flag as an lset, the way `CaLink::value` gates on it: a process
    /// made while it is down fails the holder's read. Counts the holder's
    /// processings by its link reads, and with `request_on_read` set raises
    /// one request from inside the first processing it sees.
    struct FlagLset {
        state: Arc<LinkConnState>,
        reads: AtomicU64,
        request_on_read: AtomicBool,
    }

    impl FlagLset {
        fn read(&self) -> Option<EpicsValue> {
            self.reads.fetch_add(1, Ordering::Relaxed);
            if self.request_on_read.swap(false, Ordering::Relaxed) {
                self.state.request_process();
            }
            self.state.is_set().then_some(EpicsValue::Double(42.5))
        }
    }

    #[async_trait::async_trait]
    impl epics_base_rs::server::database::LinkSet for FlagLset {
        fn is_connected(&self, _: LinkRef<'_>) -> bool {
            self.state.is_set()
        }
        async fn get_value(&self, _: LinkRef<'_>) -> Option<EpicsValue> {
            self.read()
        }
        fn get_cached_value(&self, _: LinkRef<'_>) -> Option<EpicsValue> {
            self.read()
        }
        async fn connect_link(
            &self,
            _: LinkRef<'_>,
            _: Option<epics_base_rs::server::database::CaLinkInit>,
        ) {
        }
    }

    /// A CP link's state, with no database.
    fn cp_link_state() -> LinkConnState {
        let open = epics_base_rs::server::record::OpenLatch::ca(
            epics_base_rs::server::record::LinkProcessPolicy::ChannelProcess,
            epics_base_rs::server::record::LinkFieldType::In,
        );
        LinkConnState::new(Arc::new(RwLock::new(None)), LinkRef::new(b"UP:PV", &open))
    }

    /// A link state on `UP:PV` and a database whose ai `HOLDER` reads it
    /// through a CP link served by a [`FlagLset`].
    fn cp_holder() -> (Arc<LinkConnState>, Arc<FlagLset>, PvDatabase) {
        use epics_base_rs::server::records::ai::AiRecord;
        let db_slot = Arc::new(RwLock::new(None));
        let inp = Arc::new(epics_base_rs::server::record::parse_link_v2("UP:PV CP"));
        let ext = inp.external().unwrap();
        let state = Arc::new(LinkConnState::new(
            db_slot.clone(),
            LinkRef::new(ext.name, ext.open),
        ));
        let lset = Arc::new(FlagLset {
            state: state.clone(),
            reads: AtomicU64::new(0),
            request_on_read: AtomicBool::new(false),
        });
        let db = block(async {
            let db = PvDatabase::new();
            db.register_link_set("ca", lset.clone()).await;
            db.add_record("HOLDER", Box::new(AiRecord::new(0.0)))
                .await
                .unwrap();
            {
                let rec = db.get_record("HOLDER").unwrap();
                let mut inst = rec.write();
                inst.common.text.inp = "UP:PV CP".into();
                inst.install_link_cache("INP", inp).expect("a common link");
            }
            db.initialize_link_locality().await;
            db.setup_cp_links().await;
            // C `dbCaRun`: the processor services nothing before it.
            db.process_cp_links_on_connect().await;
            db
        });
        *db_slot.write() = Some(db.clone());
        (state, lset, db)
    }

    /// `worker` polled until it waits for the next request.
    fn run_to_wait(worker: impl std::future::Future<Output = ()>) {
        let worker = std::pin::pin!(worker);
        let mut cx = std::task::Context::from_waker(std::task::Waker::noop());
        assert!(worker.poll(&mut cx).is_pending());
    }

    /// `state`'s processor, stepped by hand: every request it holds is taken.
    fn step(state: &LinkConnState) {
        run_to_wait(state.process.run_worker(|| state.db.read().clone()));
    }

    /// One processing of `HOLDER`'s link, taken by a worker of its own.
    fn process_once(db: &PvDatabase) {
        let (_, _, inp) = db.record_link_fields("HOLDER").remove(0);
        let ext = inp.external().unwrap();
        let process = ExternalCpProcess::new(LinkRef::new(ext.name, ext.open));
        process.request();
        run_to_wait(process.run_worker(|| Some(db.clone())));
    }

    /// An open the link's removal overtook starts no channel and no task:
    /// `dbCaTask` serves `CA_CLEAR_CHANNEL` first (`dbCa.c:1200`).
    #[cfg(tokio_backend)]
    #[tokio::test(flavor = "current_thread")]
    async fn an_open_its_removal_overtook_starts_nothing() {
        use epics_base_rs::server::records::ai::AiRecord;
        let resolver = CaLinkResolver::new(task::Reactor::current().expect("test runtime"));
        resolver.client().await.expect("a client");
        let db = PvDatabase::new();
        db.add_record("H", Box::new(AiRecord::new(0.0)))
            .await
            .unwrap();
        let inp = Arc::new(epics_base_rs::server::record::parse_link_v2("UP:PV CP"));
        {
            let rec = db.get_record("H").unwrap();
            let mut inst = rec.write();
            inst.common.text.inp = "UP:PV CP".into();
            inst.install_link_cache("INP", inp.clone())
                .expect("a common link");
        }
        db.put_record_field_from_ca_no_notify("H", "INP", EpicsValue::String("0".into()))
            .await
            .unwrap();
        let ext = inp.external().unwrap();
        assert!(ext.open.is_removed());

        // Current-thread: no task the open spawns runs, or is reaped, before
        // the count below.
        let tasks = || {
            tokio::runtime::Handle::current()
                .metrics()
                .num_alive_tasks()
        };
        let before = tasks();
        assert!(
            resolver
                .open_link(LinkRef::new(ext.name, ext.open))
                .await
                .is_none()
        );
        assert_eq!(tasks(), before, "the removed link's open started a task");
    }

    /// A first event whose pass ran before the link's CP edge was published
    /// still processes the holder (`dbCa.c:958-962`): the relink that
    /// publishes the edge joins the link, and the join repeats the pass.
    #[cfg(tokio_backend)]
    #[tokio::test(flavor = "current_thread")]
    async fn a_pass_that_found_no_edge_is_owed_to_the_join() {
        let (db, _) = epics_base_rs::server::ioc_builder::IocBuilder::new()
            .db_string(
                r#"record(ai, "H") { field(FLNK, "CNT") }
                   record(calc, "CNT") { field(CALC, "VAL+1") }"#,
                &HashMap::new(),
            )
            .unwrap()
            .build()
            .await
            .unwrap();
        let inp = Arc::new(epics_base_rs::server::record::parse_link_v2("UP:PV CP"));
        {
            let rec = db.get_record("H").unwrap();
            let mut inst = rec.write();
            inst.common.text.inp = "UP:PV CP".into();
            inst.install_link_cache("INP", inp.clone())
                .expect("a common link");
        }
        let ext = inp.external().unwrap();
        let link = LinkRef::new(ext.name, ext.open);
        let resolver = CaLinkResolver::new(task::Reactor::current().expect("test runtime"));
        // Opened with no database: no worker, the test steps the processor.
        let opened = resolver.open_link(link).await.expect("an open");
        resolver.attach_database((*db).clone());
        db.process_cp_links_on_connect().await;
        let processes = || db.get_pv("CNT").unwrap().to_f64().unwrap();

        opened.connected.request_process();
        step(&opened.connected);
        assert_eq!(processes(), 0.0, "a pass with no edge processed H");

        db.setup_cp_links().await;
        resolver.join_cp_link(link);
        step(&opened.connected);
        assert_eq!(processes(), 1.0, "the join dropped the missed pass");
        resolver.join_cp_link(link);
        step(&opened.connected);
        assert_eq!(processes(), 1.0, "the miss is owed once");
    }

    /// C `accessRightsCallback` only adds `CA_DBPROCESS` (`dbCa.c:1029`):
    /// a rights change processes the holder when the processor takes it,
    /// and two before it runs are one process.
    #[test]
    fn a_rights_change_processes_the_cp_holder_only_on_the_processor() {
        let (state, lset, db) = cp_holder();
        let reads = || lset.reads.load(Ordering::Relaxed);
        let base = reads();
        process_once(&db);
        let one = reads() - base;
        assert!(one > 0);
        state.mark_connected();

        let base = reads();
        assert!(state.note_access_rights(false, false));
        assert!(state.note_access_rights(true, false));
        assert_eq!(reads(), base, "the rights callback processed inline");
        step(&state);
        assert_eq!(reads() - base, one);
        step(&state);
        assert_eq!(reads() - base, one, "both were taken");
    }

    /// C `eventCallback` only adds `CA_DBPROCESS` (`dbCa.c:958-962`): a
    /// monitor event processes the holder when the processor takes it, and
    /// two before it runs are one process.
    #[test]
    fn a_monitor_event_processes_the_cp_holder_only_on_the_processor() {
        let (state, lset, db) = cp_holder();
        let reads = || lset.reads.load(Ordering::Relaxed);
        let base = reads();
        process_once(&db);
        let one = reads() - base;
        assert!(one > 0);
        let events = LinkEvents::new();
        let event = || {
            Snapshot::new(
                EpicsValue::Double(1.0),
                0,
                0,
                std::time::SystemTime::UNIX_EPOCH,
            )
        };

        let base = reads();
        note_monitor_value(&events, &state, event(), 1);
        note_monitor_value(&events, &state, event(), 1);
        assert_eq!(reads(), base, "the monitor callback processed inline");
        step(&state);
        assert_eq!(reads() - base, one);
        step(&state);
        assert_eq!(reads() - base, one, "both were taken");
    }

    /// C `addAction` (`dbCa.c:132-161`) per boundary, with the processor
    /// stepped by hand: requests raised before it runs are one process, and
    /// one raised during the process is a second.
    #[test]
    fn process_requests_merge_until_the_processor_takes_them() {
        let (state, lset, db) = cp_holder();
        let reads = || lset.reads.load(Ordering::Relaxed);
        let base = reads();
        step(&state);
        assert_eq!(reads(), base, "nothing requested");
        // What one processing of the holder reads, the unit counted below.
        let base = reads();
        process_once(&db);
        let one = reads() - base;
        assert!(one > 0);

        let base = reads();
        state.request_process();
        state.request_process();
        step(&state);
        assert_eq!(reads() - base, one, "two requests before the processor ran");
        step(&state);
        assert_eq!(reads() - base, one, "both were taken");

        lset.request_on_read.store(true, Ordering::Relaxed);
        state.request_process();
        step(&state);
        assert_eq!(
            reads() - base,
            3 * one,
            "the request made while processing is a second process"
        );
        step(&state);
        assert_eq!(reads() - base, 3 * one);
    }

    /// An outage that began and ended inside a lag: the receiver sees only
    /// `Lagged` and what came after, the channel is connected again, and
    /// the replayed edge still requests the CP holder's process. The
    /// processor runs after the reconnect, so the holder reads the
    /// reconnected link with no alarm, C's outcome when dbCaTask runs after
    /// `connectionCallback`'s reconnect (`dbCa.c:820-826`, `:1314-1321`).
    #[test]
    fn an_outage_lost_in_a_lag_processes_the_cp_holder_after_the_reconnect() {
        use epics_base_rs::runtime::sync::broadcast;
        let (state, lset, db) = cp_holder();
        let reads = || lset.reads.load(Ordering::Relaxed);
        let alarm = || {
            let rec = db.get_record("HOLDER").unwrap();
            let inst = rec.read();
            (inst.common.stat, inst.common.sevr as u16)
        };

        state.sync(fold(true, RW, 0));
        let before = reads();
        process_once(&db);
        let one = reads() - before;
        let before = reads();

        let (tx, mut rx) = broadcast::channel(2);
        for evt in [
            ConnectionEvent::Disconnected,
            ConnectionEvent::AccessRightsChanged {
                read: false,
                write: false,
            },
            ConnectionEvent::AccessRightsChanged {
                read: true,
                write: true,
            },
            ConnectionEvent::Connected,
        ] {
            tx.send(evt).unwrap();
        }
        let now = fold(true, RW, 1);
        assert_eq!(
            block(watch_step(&mut rx, &state, || now)),
            WatchStep::Refetch
        );
        assert_eq!(state.disconnect_count(), 1);
        assert!(state.is_set());

        step(&state);
        assert_eq!(reads(), before + one, "the lost outage requested a process");
        assert_eq!(alarm(), (0, 0), "the holder read the reconnected link");

        // The retained tail wakes the watcher again; the outage is counted.
        assert_eq!(
            block(watch_step(&mut rx, &state, || now)),
            WatchStep::Applied
        );
        assert_eq!(state.disconnect_count(), 1);
        step(&state);
        assert_eq!(reads(), before + one);
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
        let state = cp_link_state();

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
    /// `dbCa.c:835-859` / the `dbCaGetLink` invalid-cache path
    /// (`dbCa.c:455-464`). Tests the type/count gate directly — the
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

    /// Stage C3 guard, mirroring the PVA connection-scope guard
    /// (`epics-pva-rs/src/server_native/tcp.rs`
    /// `connection_scope_spawns_go_through_the_runtime_seam`): every task
    /// the `calink` production surface spawns must go through a
    /// [`task::Reactor`] the caller handed in, never a bare `tokio::spawn`,
    /// a `tokio::runtime::Handle::spawn`, or a `tokio::runtime::Handle`
    /// field pinned into a production type. A bare `tokio::spawn` panics
    /// on a thread with no tokio runtime — which is exactly the
    /// callback-band worker the RTEMS exec backend runs these tasks on —
    /// and it panics at *runtime*, on the target, not here. So pin it as
    /// source inspection over the whole calink module (`resolver.rs`,
    /// `mod.rs`, `iocsh.rs`).
    ///
    /// The needle moved with the seam: it used to be the free
    /// `runtime::task::spawn(`, which is gone — a spawn now goes through a
    /// `Reactor` value, so the positive check is `reactor.spawn(` and
    /// `Reactor::current(` is banned outright, calink taking its executor
    /// from `PvaLinkResolver`'s owner rather than reading one out of the
    /// calling thread.
    ///
    /// Before Stage C3 the CA client passed this guard and calink did
    /// not (design doc §5.1, §6 Stage C3); that asymmetry is what the
    /// guard exists to close and keep closed.
    ///
    /// The needles are assembled with `concat!` so this test's own source
    /// text cannot satisfy the check it performs.
    #[test]
    fn calink_production_spawns_go_through_the_runtime_seam() {
        let resolver = production(include_str!("resolver.rs"), Comments::Strip);
        let module = production(include_str!("mod.rs"), Comments::Strip);
        let iocsh = production(include_str!("iocsh.rs"), Comments::Strip);

        // Fail closed: an earlier `#[cfg(test)]` helper must not shrink a
        // slice past the code this guard is meant to cover.
        assert!(
            resolver.contains("async fn open_link"),
            "resolver production slice no longer covers the link-open path"
        );
        assert!(
            module.contains("calink_link_set_install"),
            "mod production slice no longer covers the link-set installer"
        );
        assert!(
            iocsh.contains("db_dbcaxr_command"),
            "iocsh production slice no longer covers the dbcaxr command"
        );

        // Positive: calink production actually spawns through a held
        // capability.
        assert!(
            resolver.contains(concat!("reactor", ".spawn(")),
            "resolver production must spawn through a held `Reactor`"
        );

        // Negative: none of the four forbidden shapes appear in any calink
        // production slice. `Reactor::current(` joins the list because a
        // mint inside calink would re-create the ambient read the capability
        // exists to remove — calink's executor comes from its owner.
        let bare_spawn = concat!("tokio", "::spawn(");
        let handle_spawn = concat!("handle", ".spawn(");
        let handle_type = concat!("tokio::runtime", "::Handle");
        let ambient_mint = concat!("Reactor", "::current(");
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
            assert_eq!(
                src.matches(ambient_mint).count(),
                0,
                "{name}: production must take its executor from its owner, not \
                 mint one with `{ambient_mint}`"
            );
        }
    }
}
