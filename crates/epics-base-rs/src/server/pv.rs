use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use crate::runtime::sync::PriorityInheritanceMutex;

use crate::error::CaError;
use crate::server::event_queue::{EventReader, EventSink, EventUser, PostOutcome, TryRecvError};
use crate::server::snapshot::{ControlInfo, DisplayInfo, EnumInfo, PropertySupport, Snapshot};
use crate::types::{DbFieldType, EpicsValue, WallTime};

/// Per-PV subscriber cap. Default 1024 — comfortably above
/// any realistic dashboard fan-out, small enough to bound the
/// per-PV `Vec<Subscriber>` under abuse. Override via
/// `EPICS_CAS_MAX_SUBSCRIBERS_PER_PV`.
pub(crate) fn max_subscribers_per_pv() -> usize {
    crate::runtime::env::get("EPICS_CAS_MAX_SUBSCRIBERS_PER_PV")
        .and_then(|s| s.parse::<usize>().ok())
        .unwrap_or(1024)
        .max(8)
}

/// Process-global counter of monitor events the subscriber never observed
/// because a later post replaced them in the event queue — C `evSubscrip
/// ::nreplace` (`dbEvent.c:821`), summed over every monitor. Covers both
/// `ProcessVariable` and `RecordInstance` posts, because both reach the queue
/// through the single [`EventSink::post`] owner. Mirrors the pattern of
/// `dropped_monitors` on the client side (subscribe_with_deadband).
///
/// read via [`dropped_monitor_events`]. That reader is not yet
/// wired to a live scrape surface — the `/queues` admin endpoint
/// currently renders configured limits only, not this counter — so do
/// not assume the value is observable through an endpoint until that
/// wiring lands.
static DROPPED_MONITOR_EVENTS: AtomicU64 = AtomicU64::new(0);

/// Read the cumulative count of dropped monitor events. Intended for
/// introspection / metrics; see `DROPPED_MONITOR_EVENTS` for the
/// current wiring status.
pub fn dropped_monitor_events() -> u64 {
    DROPPED_MONITOR_EVENTS.load(Ordering::Relaxed)
}

/// Identity of the client driving a `WriteHook` invocation. Carries
/// the user/host/peer fields the CA TCP handler already tracks for
/// audit + access security, so a proxy hook (gateway, ACL filter,
/// putlog) can make decisions without re-deriving them.
#[derive(Debug, Clone, Default)]
pub struct WriteContext {
    /// CA `CLIENT_NAME` username, or empty if unknown.
    pub user: String,
    /// CA `HOST_NAME` hostname (or peer IP fallback), used for ACF
    /// matching against `HAG(...)` groups.
    pub host: String,
    /// Raw `peer.ip():peer.port()` string, retained for audit/log use.
    pub peer: String,
}

/// Async hook invoked by client-originated writes (CA `caput`, CA
/// `WRITE_NOTIFY`) before the PV's local value is set. Used by the CA
/// gateway and similar proxies to forward writes upstream instead of
/// landing them in the local `ProcessVariable`.
///
/// The hook receives the proposed new value plus a [`WriteContext`]
/// identifying the client, and must return either:
/// * `Ok(())` — the write was accepted (e.g. forwarded to upstream).
///   The caller does NOT update the local `value` field — the
///   subsequent upstream-monitor event is expected to do that. This
///   matches CA-gateway semantics where the cached value reflects
///   reality after the round-trip.
/// * `Err(CaError)` — the write was rejected. The caller surfaces
///   the error to the CA client (`WRITE_NOTIFY` carries the ECA
///   status). The hook itself decides whether to update local state
///   on rejection.
///
/// The hook is consulted only on the client → server path. Internal
/// callers (`ProcessVariable::set`, `put_pv_and_post`) bypass it so
/// the upstream-monitor forwarder can update local state without
/// recursing into itself.
///
/// ## Stale-local hazard
///
/// "Hook returns `Ok` → caller does NOT update local value" assumes
/// the upstream will emit a monitor event reflecting the new value.
/// EPICS records can violate that assumption: PP=NO fields,
/// PUT-only fields (e.g. `.PROC`), and records configured to suppress
/// monitor events on identical values. In those cases the shadow
/// PV remains at its pre-put value indefinitely — caput appears to
/// succeed but `caget` afterwards returns the old value.
///
/// Hook implementors who target such records SHOULD update the local
/// `ProcessVariable` themselves on `Ok` — typically by invoking
/// `pv.set(new_value).await` AFTER the upstream put-ack, accepting
/// the cost of one local mutation per put. The base hook contract
/// stays "do nothing on Ok" because most monitor-driven shadows
/// (the CA gateway's primary use case) WILL receive a monitor event
/// and updating locally would race with it.
///
/// ## Reentrancy
///
/// The TCP write path clones the hook `Arc` and releases the read
/// guard BEFORE invoking it, so a hook that calls
/// `pv.set_write_hook(...)` to swap itself does not deadlock. A hook
/// that calls `pv.set(...)` reentrantly is allowed but defeats the
/// "let the upstream-monitor update local state" contract — the
/// reentrant `set` will be silently overwritten by the next
/// upstream event.
pub type WriteHook = Arc<
    dyn Fn(
            EpicsValue,
            WriteContext,
        )
            -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), CaError>> + Send>>
        + Send
        + Sync,
>;

/// read/write access decision for a gateway shadow PV,
/// evaluated for a specific downstream `(user, host)`. Mirrors the CA
/// access-rights model the server reports to the client and gates
/// reads on.
#[derive(Debug, Clone, Copy)]
pub struct AccessDecision {
    /// Client may GET / MONITOR (`EVENT_ADD`) the PV.
    pub read: bool,
    /// Client may PUT (`WRITE` / `WRITE_NOTIFY`) the PV.
    pub write: bool,
}

/// per-PV access hook installed by a proxy (the CA
/// gateway) so the CA server routes a shadow PV's access-rights
/// decision through the proxy's own ACF instead of the server's.
/// Given the downstream client's `(user, host)`, it returns the
/// [`AccessDecision`].
///
/// Symmetric to [`WriteHook`]: the gateway captures its single
/// `ArcSwap<AccessConfig>` and the PV's `.pvlist` ASG/ASL in the
/// closure, so `compute_access` reports access rights and gates reads
/// with the same `can_read` / `can_write` the write hook uses — one
/// ACF authority, no second copy to keep in sync. The hook is
/// synchronous (it only reads an in-memory `ArcSwap`, no `.await`); the
/// server consults it at `CREATE_CHAN` and on access-rights
/// re-evaluation.
pub type AccessHook = Arc<dyn Fn(&str, &str) -> AccessDecision + Send + Sync>;

/// per-PV read hook consulted by the CA server's one-shot GET path
/// (`CA_PROTO_READ` / `CA_PROTO_READ_NOTIFY`) when set. A bare PV serves
/// reads straight from its stored value cell; a proxy (the CA gateway in
/// its no-cache mode) installs this hook so each downstream GET is
/// satisfied by a *fresh* upstream fetch instead of the last cached
/// value. Mirrors C ca-gateway `-no_cache`, where a connected channel
/// with caching disabled forwards every read as a fresh
/// `ca_array_get_callback()` to the IOC (`gateVc.cc:1361-1369`) rather
/// than returning `vc->eventData()`.
///
/// The hook returns a full [`Snapshot`], not a bare value: C `-no_cache`
/// reads issue `ca_array_get_callback(eventType(), ...)` with `eventType()`
/// a `DBR_TIME_*` class, and `getTimeCB` decodes the event's status,
/// severity, and timestamp into `setEventData` before the GET completes
/// (`gatePv.cc:976`, `:1789-1794`). The hook therefore owns producing the
/// fresh value *together with* its upstream alarm/timestamp so the read
/// path never synthesizes metadata by grafting a fresh value onto an
/// unrelated cached snapshot. Property metadata (display/control/enum) is
/// not carried by a `DBR_TIME_*` event in either C or here; the consumer
/// overlays the shadow's last-known property metadata for those fields
/// (a separate upstream path feeds them, as C splits value/time from the
/// property monitor).
///
/// The hook is async (it performs an upstream get) and fallible: on
/// `Err` the server surfaces the failure to the client (`ECA_GETFAIL`)
/// exactly as the IOC's own get-callback error would propagate. Only the
/// GET path consults it ([`ProcessVariable::read_snapshot`]); monitor
/// fan-out, the initial monitor event, and access-rights re-posts keep
/// serving the stored snapshot, so a no-cache PV still backs a downstream
/// monitor with its upstream subscription's events.
///
/// `None` (the default) leaves the read path byte-for-byte unchanged for
/// every record-backed and cached PV — the hook is purely additive.
pub type ReadHook = Arc<
    dyn Fn()
            -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<Snapshot, CaError>> + Send>>
        + Send
        + Sync,
>;

/// A monitor event sent to subscribers when a PV value changes.
/// Carries a full Snapshot so GR/CTRL metadata (PREC, EGU, limits) is available.
#[derive(Debug, Clone)]
pub struct MonitorEvent {
    pub snapshot: Snapshot,
    /// Origin writer ID. When non-zero, subscribers with the same
    /// `ignore_origin` can filter out self-triggered events.
    /// Used to prevent sequencer write-back loops.
    ///
    /// **Scope**: tagged explicitly by the `put_*_post` tier
    /// (`put_pv_and_post_with_origin`, records and simple PVs alike), and
    /// inherited by every post inside the `put_*_process` tier's
    /// synchronous put+process cascade through the thread-local ambient
    /// write origin (`AmbientWriteOriginScope`) — both record funnels
    /// (`notify_field_with_origin`, `notify_from_snapshot`) and the
    /// simple-PV funnel (`ProcessVariable::deliver`) apply the same
    /// inheritance rule. Posts from work a cascade merely spawned (async
    /// record completions, driver pollers) run outside any scope and stay
    /// origin 0.
    pub origin: u64,
    /// The `DBE_*` event class(es) this post carries — C attaches the
    /// posting mask to each event's field log (`db_field_log.mask`,
    /// dbEvent.c) and pvxs narrows per event from `pDbFieldLog->mask`
    /// (`groupsource.cpp:331-337`). Producer-side it was already used to
    /// gate delivery (`Subscriber::accepts`); carrying it on the event
    /// lets subscribers narrow what they decode (e.g. a QSRV group
    /// monitor updating only alarm leaves on a `DBE_ALARM`-only event).
    /// When events coalesce under a slow consumer, masks accumulate by
    /// OR — the surviving snapshot is the newest, the mask reports every
    /// class that changed since the last delivered event.
    pub mask: crate::server::recgbl::EventMask,
}

/// A subscriber waiting for PV value updates — C `evSubscrip`'s producer-side
/// view. Its pending events live in the shared event queue
/// ([`crate::server::event_queue`]), reached only through `sink`.
pub struct Subscriber {
    pub sid: u32,
    pub data_type: DbFieldType,
    pub mask: u16,
    /// Producer half of this monitor's slot in the circuit's event queue.
    /// `pub(crate)` so no code outside this crate can enqueue past the
    /// append-vs-replace rule the queue owns.
    pub(crate) sink: EventSink,
    /// Server-side channel filter chain (epics-base 3.15.7).
    /// Defaults to empty — every event passes unchanged. Populated
    /// by the subscription path when the channel name carries a
    /// `.{filter:opts}` JSON suffix (`dbnd`, `arr`, `ts`, ...).
    pub filters: crate::server::database::filters::FilterChain,
    /// Delivery gate. `true` (the default) delivers events normally;
    /// `false` suppresses every post to this subscriber at the source —
    /// nothing reaches the event queue, no filter is evaluated — so a
    /// paused monitor stops the record-event work entirely, not just the
    /// downstream frame. Mirrors EPICS `db_event_disable` / pvxs
    /// `onStart(false)` (singlesource.cpp:151-173, groupsource.cpp:151-281):
    /// the subscription object survives, only its event flow is gated, so
    /// the same subscriber resumes on re-enable. Flipped only under the
    /// owner's write lock via [`super::record::record_instance::RecordInstance::set_subscriber_active`]
    /// (records) — the post paths read it under the matching read lock.
    pub active: bool,
}

impl Subscriber {
    /// a monitor delivery is gated on the requested `DBE_*`
    /// mask. Returns true only when the post's event class intersects
    /// this subscriber's mask — the single rule C rsrv enforces with
    /// `caEventMask & pevent->select` (`dbEvent.c:892-900`) and the
    /// same intersection the record-field monitor path applies. An
    /// empty post class (no specific class) delivers unconditionally.
    fn accepts(&self, post: crate::server::recgbl::EventMask) -> bool {
        post.is_empty() || crate::server::recgbl::EventMask::from_bits(self.mask).intersects(post)
    }

    /// The single post path for both event sources (`ProcessVariable` value /
    /// alarm / property posts and `RecordInstance` field monitors): hand the
    /// event to this monitor's event queue, which owns C's append-vs-replace
    /// decision (`db_queue_event_log`), and apply the one piece of accounting
    /// that lives outside the queue — the counter for a value that a later post
    /// displaced before the consumer ever saw it (C `nreplace`).
    pub(crate) fn post(&self, event: MonitorEvent) {
        if self.sink.post(event) == PostOutcome::Replaced {
            DROPPED_MONITOR_EVENTS.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// The consumer for this monitor is gone; the producer row can be reaped.
    pub(crate) fn is_closed(&self) -> bool {
        self.sink.is_closed()
    }
}

/// Shadow `DBR_GR_*` / `DBR_CTRL_*` / enum metadata for a
/// non-record-backed PV.
///
/// A bare [`ProcessVariable`] has no record engine to derive units /
/// precision / display+alarm+control limits / enum labels from, so a
/// proxy that fronts an upstream IOC (the CA / PVA gateway) fetches the
/// upstream's control metadata and installs it here via
/// [`ProcessVariable::set_metadata`]. Every snapshot the PV emits —
/// the GET path ([`ProcessVariable::snapshot`]) and every monitor
/// event ([`ProcessVariable::post_property`], value, alarm, and
/// gateway snapshot posts) — then carries it, so a downstream client
/// that requested a `DBR_GR_*` / `DBR_CTRL_*` type receives the
/// upstream metadata instead of zeroed limits.
///
/// Mirrors the C ca-gateway, where `gatePvData` subscribes to
/// `DBE_PROPERTY` and issues a control-type `ca_array_get_callback`
/// (`gatePv.cc:850-934`) then copies units / precision / graphic +
/// control limits into the gateway's gdd attributes
/// (`gatePv.cc:1916-2007`).
#[derive(Debug, Clone, Default)]
pub struct PvMetadata {
    pub display: Option<DisplayInfo>,
    pub control: Option<ControlInfo>,
    pub enums: Option<EnumInfo>,
}

/// Metadata of the most recent full-snapshot write to a bare PV:
/// alarm + acquisition timestamp + userTag. A bare `ProcessVariable`
/// has no alarm engine, so without this it would forget everything a
/// full-value write carried beyond the raw value. pvxs mailbox
/// `SharedPV::post()` assigns the *whole* posted value to the current
/// value (`sharedpv.cpp:417-432`); to match that, a PV that received a
/// full posted Value must reflect its alarm/time on every later GET,
/// not just to the monitor subscribers that saw the post live.
#[derive(Clone)]
struct PostedMeta {
    alarm: crate::server::snapshot::AlarmInfo,
    timestamp: WallTime,
    user_tag: i32,
}

/// A process variable hosted by the server.
pub struct ProcessVariable {
    pub name: String,
    /// The stored value. A synchronous `parking_lot::RwLock` (matching the
    /// sibling `posted_meta` / `metadata` / hook locks): every access is a
    /// single-expression read-or-write with no `.await` held across the
    /// guard, so the value-read path (`get`, `snapshot`) is pure lock work
    /// with no reactor dependency — the sans-io READ path. The write side
    /// (`set` / `set_snapshot`) still `.await`s the monitor fan-out, but
    /// drops this guard first.
    pub value: parking_lot::RwLock<EpicsValue>,
    /// Monitor fan-out list — **L7** of `doc/rtems-priority-locks-design.md`
    /// §3.
    ///
    /// A BLOCKING mutex, not the async one: every emission path runs from a
    /// record-processing thread with the record's advisory gate (L1) held, and
    /// C's `db_post_events` likewise takes `evUser->lock` from inside
    /// `dbScanLock` (`dbEvent.c::db_post_events`). Holding an async mutex here
    /// would put a suspension point inside that window. Every critical section
    /// below is bounded list work (`retain` / `push` / `sub.post`), with no
    /// I/O and no `.await` inside it.
    ///
    /// Specifically a [`PriorityInheritanceMutex`] rather than a plain
    /// `parking_lot::Mutex`, because `evUser->lock` is an `epicsMutex` and on
    /// the RTEMS arm every `epicsMutex` is a `PTHREAD_PRIO_INHERIT` pthread
    /// mutex (`os/posix/osdMutex.c:71-88`, compiled for RTEMS via
    /// `os/RTEMS-posix/osdMutex.c:8`). It is taken from banded IOC threads on
    /// both sides — the emitting record-processing thread and a `CAS-client`
    /// thread running `remove_subscriber` — so a plain mutex here reintroduces
    /// the inversion L1 was converted to remove. Off the PI targets this is
    /// `parking_lot::Mutex`, i.e. exactly what it was.
    ///
    /// A leaf of the acquisition order (`record_lock.rs` module doc): no other
    /// lock is taken while it is held.
    pub subscribers: PriorityInheritanceMutex<Vec<Subscriber>>,
    /// Sticky metadata of the last full-snapshot write. `None` until a
    /// [`Self::set_snapshot`] lands; a value-only [`Self::set`] clears it
    /// back to `None` (a plain value write carries no explicit
    /// alarm/time, so it reverts to NO_ALARM + wall-clock-now). When
    /// `Some`, [`Self::snapshot`] serves these instead of the defaults.
    /// Single meaning: the served snapshot reflects the most recent
    /// write — value always current, metadata from that write.
    posted_meta: parking_lot::RwLock<Option<PostedMeta>>,
    /// Shadow DBR_GR_*/DBR_CTRL_*/enum metadata, installed by a proxy
    /// (CA / PVA gateway) via [`Self::set_metadata`]. Empty for a plain
    /// local PV. Stored under the same sync `parking_lot::RwLock` slot
    /// rationale as the hooks: every snapshot builder reads it without
    /// an `.await`. See [`PvMetadata`].
    metadata: parking_lot::RwLock<PvMetadata>,
    /// Optional hook consulted on client-originated writes. When set,
    /// the CA TCP write path delegates to the hook instead of doing a
    /// local `pv.set()`. See [`WriteHook`].
    ///
    /// Stored under `parking_lot::RwLock` (sync) rather than the
    /// async `tokio::sync::RwLock` so the hot put-path can read it
    /// without an `.await` round-trip — `write_hook()` is now a
    /// constant-time clone of the optional `Arc`. The hook itself
    /// is async (returns a `Future`); only the slot is sync.
    write_hook: parking_lot::RwLock<Option<WriteHook>>,
    /// optional access hook consulted by the CA server's
    /// `compute_access` to decide a downstream client's read/write
    /// rights for this PV. When set, it overrides the server's own ACF
    /// for this PV — the gateway uses it to enforce `.pvlist` ASG-based
    /// `can_read` / `can_write`, symmetric to [`Self::write_hook`].
    /// Same sync `parking_lot::RwLock` slot rationale as `write_hook`.
    access_hook: parking_lot::RwLock<Option<AccessHook>>,
    /// optional read hook consulted by the CA server's one-shot GET path
    /// ([`Self::read_snapshot`]) to fetch a fresh value instead of the
    /// stored cell. Used by the CA gateway's no-cache mode to forward
    /// each downstream GET to upstream. `None` (the default) keeps the
    /// read path serving the stored value, identical to before. Same
    /// sync slot rationale as [`Self::write_hook`]: the GET path clones
    /// the optional `Arc` without an `.await`, then awaits the hook
    /// outside any lock. See [`ReadHook`].
    read_hook: parking_lot::RwLock<Option<ReadHook>>,
}

impl ProcessVariable {
    pub fn new(name: String, initial: EpicsValue) -> Self {
        Self {
            name,
            value: parking_lot::RwLock::new(initial),
            subscribers: PriorityInheritanceMutex::new(Vec::new()),
            metadata: parking_lot::RwLock::new(PvMetadata::default()),
            posted_meta: parking_lot::RwLock::new(None),
            write_hook: parking_lot::RwLock::new(None),
            access_hook: parking_lot::RwLock::new(None),
            read_hook: parking_lot::RwLock::new(None),
        }
    }

    /// Install (or replace) the shadow DBR_GR_*/DBR_CTRL_*/enum
    /// metadata served on this PV's snapshots. Used by the CA / PVA
    /// gateway after fetching the upstream IOC's control metadata. See
    /// [`PvMetadata`]. To publish the change to downstream property
    /// monitors, follow with [`Self::post_property`].
    pub fn set_metadata(&self, metadata: PvMetadata) {
        *self.metadata.write() = metadata;
    }

    /// Snapshot (clone) of the installed shadow metadata; empty
    /// (`Default`) for a plain local PV.
    pub fn metadata(&self) -> PvMetadata {
        self.metadata.read().clone()
    }

    /// Fill any metadata field the snapshot leaves `None` from the
    /// installed shadow metadata. A field the caller already populated
    /// (e.g. a gateway snapshot that carried its own metadata) wins —
    /// this only supplies what is otherwise absent, so every emission
    /// path serves the upstream metadata uniformly without clobbering a
    /// richer source.
    fn apply_metadata(&self, snap: &mut Snapshot) {
        let meta = self.metadata.read();
        if snap.display.is_none() {
            snap.display = meta.display.clone();
        }
        if snap.control.is_none() {
            snap.control = meta.control.clone();
        }
        if snap.enums.is_none() {
            snap.enums = meta.enums.clone();
        }
        // A bare PV has no `rset`, so "which properties does this channel
        // supply" is answered by what metadata it actually HAS: a proxy that
        // shadowed an upstream IOC's display/control/enum info supplies those
        // properties, a mailbox PV that nobody gave metadata to supplies none.
        // Assigned here, in the one owner of a bare PV's metadata, so the mask
        // and the values it describes cannot disagree.
        // See [`crate::server::snapshot::PropertySupport`].
        snap.properties = PropertySupport {
            units: snap.display.is_some(),
            precision: snap.display.is_some(),
            graphic_double: snap.display.is_some(),
            alarm_double: snap.display.is_some(),
            control_double: snap.control.is_some(),
            enum_strs: snap.enums.is_some(),
        }
        .narrowed_to_field(snap.value.db_field_type(), false);
    }

    /// Install an access hook. Replaces any previously
    /// installed hook.
    pub fn set_access_hook(&self, hook: AccessHook) {
        *self.access_hook.write() = Some(hook);
    }

    /// Snapshot of the installed access hook (clone of the `Arc`), or
    /// `None`. Consulted by the CA server's `compute_access`; cheap and
    /// non-async, like [`Self::write_hook`].
    pub fn access_hook(&self) -> Option<AccessHook> {
        self.access_hook.read().clone()
    }

    /// Install a read hook. Replaces any previously-installed hook.
    /// Used by the CA gateway's no-cache mode so each downstream GET is
    /// served by a fresh upstream fetch. See [`ReadHook`].
    pub fn set_read_hook(&self, hook: ReadHook) {
        *self.read_hook.write() = Some(hook);
    }

    /// Snapshot of the installed read hook (clone of the `Arc`), or
    /// `None`. Cheap and non-async, like [`Self::write_hook`]: the read
    /// lock is released before the cloned `Arc` returns, so the caller's
    /// subsequent `await` on the hook holds no lock.
    pub fn read_hook(&self) -> Option<ReadHook> {
        self.read_hook.read().clone()
    }

    /// Install a write hook. Replaces any previously-installed hook.
    pub fn set_write_hook(&self, hook: WriteHook) {
        *self.write_hook.write() = Some(hook);
    }

    /// Remove any installed write hook.
    pub fn clear_write_hook(&self) {
        *self.write_hook.write() = None;
    }

    /// Snapshot of the installed write hook (clone of the `Arc`), or
    /// `None` if none. Used by the CA TCP write path; cheap and
    /// non-async — the read lock is released before the cloned `Arc`
    /// returns, so the caller's subsequent `await` on the hook does
    /// not hold any lock.
    pub fn write_hook(&self) -> Option<WriteHook> {
        self.write_hook.read().clone()
    }

    /// Get the current value.
    ///
    /// Synchronous: a single-expression read-lock clone with no `.await`,
    /// so the value-read path carries no reactor dependency (sans-io).
    pub fn get(&self) -> EpicsValue {
        self.value.read().clone()
    }

    /// Build a Snapshot for this bare PV.
    ///
    /// A `ProcessVariable` is a non-record-backed channel: it has no
    /// alarm engine, no DESC/EGU/PREC metadata and no timestamp user
    /// tag of its own. The snapshot is therefore value + `NO_ALARM` +
    /// wall-clock now, with `user_tag` = 0. Display / control / enum
    /// metadata is `None` *unless* a proxy installed it via
    /// [`Self::set_metadata`] (the CA / PVA gateway shadowing an
    /// upstream IOC) — see `Self::apply_metadata`. Record-backed
    /// channels build their snapshot via
    /// `RecordInstance::snapshot_for_field`, which carries the record's
    /// own alarm/metadata. The only path that injects a non-zero alarm
    /// onto a bare PV is [`Self::post_alarm`] (used by the gateway
    /// adapter to surface upstream disconnect).
    pub fn snapshot(&self) -> Snapshot {
        let value = self.value.read().clone();
        // Serve the sticky metadata of the last full-snapshot write if
        // one landed (pvxs mailbox parity: a posted full Value stays the
        // current value, alarm/time included); otherwise the bare-PV
        // default of NO_ALARM + wall-clock-now.
        let mut snap = match self.posted_meta.read().clone() {
            Some(m) => {
                let mut s = Snapshot::new(value, m.alarm.status, m.alarm.severity, m.timestamp);
                s.alarm.ackt = m.alarm.ackt;
                s.alarm.acks = m.alarm.acks;
                s.user_tag = m.user_tag;
                s
            }
            None => Snapshot::new(value, 0, 0, crate::runtime::time::now_wall()),
        };
        self.apply_metadata(&mut snap);
        snap
    }

    /// Build the snapshot served on a one-shot client GET
    /// (`CA_PROTO_READ` / `CA_PROTO_READ_NOTIFY`).
    ///
    /// When a [`ReadHook`] is installed (the CA gateway's no-cache mode),
    /// the snapshot is fetched fresh through the hook — value *and* its
    /// upstream alarm status/severity and IOC timestamp together — and the
    /// shadow's last-known property metadata (display/control/enum) is
    /// overlaid for the fields a `DBR_TIME_*` event does not carry; on hook
    /// error the failure propagates so the server can answer `ECA_GETFAIL`,
    /// matching C ca-gateway forwarding each read to the IOC under
    /// `-no_cache` (`gateVc.cc:1361-1369`, `gatePv.cc:976`/`:1789-1794`).
    /// Without a hook this is exactly [`Self::snapshot`] wrapped in `Ok`,
    /// so the GET path is unchanged for every record-backed and cached PV.
    ///
    /// Only the GET path calls this; monitor fan-out, the initial monitor
    /// event, and access-rights re-posts keep using [`Self::snapshot`]
    /// (the stored value), so a no-cache PV still backs a downstream
    /// monitor with its upstream subscription's events rather than a
    /// per-event upstream get.
    pub async fn read_snapshot(&self) -> Result<Snapshot, CaError> {
        match self.read_hook() {
            Some(hook) => {
                // The hook issues a metadata-bearing upstream GET
                // (`DbrClass::Time`), so the returned snapshot already
                // carries the fresh value WITH its upstream alarm
                // status/severity and IOC timestamp — mirroring C
                // `getTimeCB` decoding the `DBR_TIME_*` event before
                // `setEventData`. A `DBR_TIME_*` event does not carry
                // display/control/enum metadata, so overlay the shadow's
                // last-known property metadata for those absent fields only
                // (a separate upstream path feeds it, exactly as C splits
                // the value/time path from the property monitor). Never
                // graft the fresh value onto the stored snapshot's
                // alarm/time, which may be stale or the bare-PV default.
                let mut snap = hook().await?;
                self.apply_metadata(&mut snap);
                Ok(snap)
            }
            None => Ok(self.snapshot()),
        }
    }

    /// Synchronous companion to [`Self::read_snapshot`] for the one-shot GET
    /// path (`CA_PROTO_READ` / `CA_PROTO_READ_NOTIFY`).
    ///
    /// `Some(snapshot)` when NO read hook is installed — the sans-io GET that
    /// every record-backed and cached PV takes: [`Self::snapshot`] of the
    /// stored value, produced with no `.await` and no reactor dependency.
    /// `None` when a gateway no-cache [`ReadHook`] IS installed, whose `hook()`
    /// is a genuine upstream network GET; the caller must then take the async
    /// [`Self::read_snapshot`] instead. This keeps the hook / no-hook decision
    /// in one owner, in lockstep with `read_snapshot` — the only difference is
    /// that the async fallible upstream fetch is surfaced to the caller as
    /// `None` rather than performed here.
    pub fn read_snapshot_local(&self) -> Option<Snapshot> {
        match self.read_hook() {
            Some(_) => None,
            None => Some(self.snapshot()),
        }
    }

    /// Set a new value and notify all subscribers.
    pub fn set(&self, new_value: EpicsValue) {
        self.set_with_origin(new_value, 0);
    }

    /// [`Self::set`] tagged with the writer's origin: the value post carries
    /// `origin` so an origin-aware consumer can recognise (and skip) the
    /// writer's own event — the simple-PV side of the
    /// `put_pv_and_post_with_origin` self-write contract. Origin 0 is the
    /// untagged default (never filtered).
    pub fn set_with_origin(&self, new_value: EpicsValue, origin: u64) {
        {
            let mut val = self.value.write();
            *val = new_value.clone();
        }
        // A plain value write carries no explicit alarm/time — revert to
        // the bare-PV default so a stale full-snapshot's metadata does
        // not linger on a value the client never stamped.
        *self.posted_meta.write() = None;
        self.notify_subscribers(new_value, origin);
    }

    /// Set value from a full snapshot (value + alarm + timestamp) and notify
    /// all subscribers. Used by the CA gateway forwarding task to propagate
    /// the upstream alarm status/severity and IOC timestamp to downstream
    /// monitors. Mirrors `gateVcData::setEventData` + `vcPostEvent` in the
    /// C ca-gateway: the incoming `dbr_time_xxx` GDD carries all three fields.
    pub fn set_snapshot(&self, snapshot: Snapshot) {
        {
            let mut val = self.value.write();
            *val = snapshot.value.clone();
        }
        // Persist the posted alarm/time/userTag so a later GET reflects
        // the full posted value, not just the live monitor fan-out.
        *self.posted_meta.write() = Some(PostedMeta {
            alarm: snapshot.alarm.clone(),
            timestamp: snapshot.timestamp,
            user_tag: snapshot.user_tag,
        });
        self.notify_subscribers_from_snapshot(snapshot);
    }

    /// Single delivery owner: emit `snapshot` to every live subscriber
    /// whose `DBE_*` mask intersects `post`.
    ///
    /// Every emission path ([`Self::notify_subscribers`] value posts,
    /// [`Self::post_alarm`], [`Self::notify_subscribers_from_snapshot`]
    /// gateway posts, [`Self::post_property`]) routes through here so the
    /// mask gate (`caEventMask & pevent->select`, `dbEvent.c:892-900`),
    /// the per-subscriber channel-filter chain, and the slow-consumer
    /// coalesce-overflow accounting are applied identically — one event
    /// class differs per caller, nothing else. The snapshot is built once
    /// by the caller (one timestamp per logical event) and cloned per
    /// subscriber.
    fn deliver(&self, post: crate::server::recgbl::EventMask, snapshot: Snapshot, origin: u64) {
        use crate::server::database::filters::FilteredMonitorEvent;
        // Same ambient-origin inheritance as the record funnels
        // (`notify_field_with_origin` / `notify_from_snapshot`): a post
        // carrying no origin of its own inherits the current thread's
        // ambient write origin, so a simple PV written from inside an
        // in-process writer's synchronous put cascade tags its event
        // with the writer's origin too. 0 outside any scope.
        let origin = if origin != 0 {
            origin
        } else {
            crate::server::record::ambient_write_origin()
        };
        let mut subs = self.subscribers.lock();
        // Remove subscribers whose consumer has been dropped.
        subs.retain(|sub| !sub.is_closed());
        for sub in subs.iter() {
            // Paused subscribers (`db_event_disable`) receive nothing —
            // skip before any work so a disabled monitor stops the event
            // flow at the source.
            if !sub.active {
                continue;
            }
            // Skip subscribers whose requested class does not intersect
            // this post's event class.
            if !sub.accepts(post) {
                continue;
            }
            let event = MonitorEvent {
                snapshot: snapshot.clone(),
                origin,
                mask: post,
            };
            // The channel-filter chain may suppress this event (e.g.
            // `dbnd` deadband not crossed); the event's mask tells value
            // filters whether to pass through (446e0d4a).
            let filtered = if sub.filters.is_empty() {
                Some(event)
            } else {
                sub.filters
                    .apply(FilteredMonitorEvent::new(event))
                    .map(|fe| fe.event)
            };
            let Some(event) = filtered else {
                continue;
            };
            // C `db_queue_event_log`: the queue appends, or replaces this
            // monitor's last entry in place when it is in flow control or
            // nearly full. Earlier distinct entries are never discarded.
            sub.post(event);
        }
    }

    /// Push a fresh monitor event holding the current value but with
    /// the supplied alarm severity/status. Used by the PVA / CA
    /// gateway adapter to surface upstream-disconnect to downstream
    /// monitor subscribers without dropping the simple PV (which
    /// would force every downstream client into ECA_DISCONN +
    /// reconnect storms when the upstream is just briefly
    /// unreachable). Mirrors gatePvData::death's "alarm-post"
    /// alternative discussed in the C++ ca-gateway audit.
    pub fn post_alarm(&self, severity: u16, status: u16) {
        use crate::server::recgbl::EventMask;
        let value = self.value.read().clone();
        let mut snapshot = Snapshot::new(value, status, severity, crate::runtime::time::now_wall());
        self.apply_metadata(&mut snapshot);
        // ALARM|LOG so DBE_LOG (archiver) subscribers receive alarm events.
        self.deliver(EventMask::ALARM | EventMask::LOG, snapshot, 0);
    }

    /// Post a `DBE_PROPERTY` monitor event carrying the decoded upstream
    /// CTRL event `snapshot` — its value plus the upstream status /
    /// severity and timestamp — overlaid with the installed shadow
    /// metadata, so downstream property-change monitors re-read the units /
    /// precision / limits / enum labels with the *upstream* alarm state.
    ///
    /// Used by the CA / PVA gateway when an upstream `DBE_PROPERTY` event
    /// fires (metadata changed) after it has refreshed the shadow PV via
    /// [`Self::set_metadata`]. The caller supplies the snapshot rather than
    /// this method synthesising one: C ca-gateway decodes the upstream
    /// `DBR_CTRL_*` callback and re-posts the value with `setStatSevr()`
    /// status/severity preserved (`gatePv.cc:2413-2438`,
    /// `runValueDataCB`), leaving the timestamp as the control DBR carries
    /// none — it must NOT be replaced with a fresh `NO_ALARM` /
    /// wall-clock-now snapshot just because metadata changed. Pass the
    /// timestamp the upstream value carried (the control event has none of
    /// its own); pass `status`/`severity` from the upstream CTRL payload.
    /// Property events are a distinct class from value/alarm: only
    /// `DBE_PROPERTY` subscribers receive them.
    pub async fn post_property(&self, mut snapshot: Snapshot) {
        use crate::server::recgbl::EventMask;
        self.apply_metadata(&mut snapshot);
        self.deliver(EventMask::PROPERTY, snapshot, 0);
    }

    /// Notify all subscribers of a new value, tagged with the writer's
    /// `origin` (0 = untagged).
    fn notify_subscribers(&self, value: EpicsValue, origin: u64) {
        use crate::server::recgbl::EventMask;
        let mut snapshot = Snapshot::new(value, 0, 0, crate::runtime::time::now_wall());
        self.apply_metadata(&mut snapshot);
        // VALUE|LOG so DBE_LOG (archiver) subscribers receive value events.
        self.deliver(EventMask::VALUE | EventMask::LOG, snapshot, origin);
    }

    /// Notify all subscribers using a pre-built Snapshot (value + alarm +
    /// timestamp). Used by `set_snapshot` to propagate the upstream alarm
    /// and IOC timestamp without synthesising a new zero-alarm local-time
    /// snapshot. Installed shadow metadata fills any metadata field the
    /// gateway snapshot left absent (see [`Self::apply_metadata`]).
    fn notify_subscribers_from_snapshot(&self, mut snapshot: Snapshot) {
        use crate::server::recgbl::EventMask;
        self.apply_metadata(&mut snapshot);
        // C gateway fires postEvent(VALUE|ALARM|LOG) for every
        // upstream event (gateVc.cc:374-376); match it so DBE_LOG
        // archivers and DBE_ALARM-only monitors receive gateway snapshot posts.
        self.deliver(
            EventMask::VALUE | EventMask::LOG | EventMask::ALARM,
            snapshot,
            0,
        );
    }

    /// Add an in-process subscriber, attached to an event queue of its own.
    ///
    /// C `db_add_event(ctx, ...)` puts a monitor on the queue chain of the
    /// `event_user` (client) that owns it. An in-process consumer is its own
    /// client, so it gets its own [`EventUser`] — nothing else shares its
    /// queue, and flow control (a CA circuit concept) never engages on it.
    /// The CA server, whose subscriptions DO share one circuit-wide queue, uses
    /// [`Self::add_subscriber_on`].
    pub fn add_subscriber(
        &self,
        sid: u32,
        data_type: DbFieldType,
        mask: u16,
    ) -> Option<EventReader> {
        self.add_subscriber_on(&EventUser::new(), sid, data_type, mask)
    }

    /// Add a subscriber whose events are queued on `user`'s event queue —
    /// C `db_add_event` with the circuit's `event_user` as context. Every
    /// subscription on one CA circuit shares that queue, and therefore its
    /// `nDuplicates`: a duplicate queued for one of them releases the
    /// EVENTS_OFF drain for all of them (`dbEvent.c:947`).
    ///
    /// Returns `None` when the per-PV subscriber cap is reached (defends
    /// against a misbehaving client opening many MONITOR ops against one shared
    /// PV; the per-channel cap limits channels but not subscriber rows on a
    /// single PV). Operators override it via `EPICS_CAS_MAX_SUBSCRIBERS_PER_PV`.
    pub fn add_subscriber_on(
        &self,
        user: &EventUser,
        sid: u32,
        data_type: DbFieldType,
        mask: u16,
    ) -> Option<EventReader> {
        let cap = max_subscribers_per_pv();
        let mut subs = self.subscribers.lock();
        // Reap rows whose consumer is gone BEFORE counting
        // against the cap. `notify_subscribers` / `post_alarm`
        // already retain-filter on every emission, but a PV with
        // no value changes (e.g. a static catalog entry that
        // dashboards latch onto and drop) never triggered the
        // reaper — a long-lived subscribe / disconnect storm could
        // pin the Vec at `cap` worth of dead rows and lock
        // out genuine new subscribers with a false-positive cap-
        // reached warning. Same defect class as the
        // NDPluginPva subscribe reaper (qsrv/pva_adapter.rs:247).
        subs.retain(|s| !s.is_closed());
        if subs.len() >= cap {
            tracing::warn!(
                pv = %self.name,
                live = subs.len(),
                cap,
                "PV subscriber cap reached, refusing add_subscriber"
            );
            return None;
        }
        let (sink, reader) = crate::server::event_queue::attach(user, sid);
        subs.push(Subscriber {
            sid,
            data_type,
            mask,
            sink,
            filters: crate::server::database::filters::FilterChain::new(),
            active: true,
        });
        Some(reader)
    }

    /// attach a channel-filter chain to an already-added
    /// subscriber (looked up by `sid`). The CA server first
    /// `add_subscriber`s, then attaches the chain parsed from the
    /// channel's `.{...}` suffix — symmetric with the record-field
    /// `RecordInstance::attach_filter_to_last_subscriber` path, so a
    /// `SimplePv` monitor runs the SAME filter chain as a record-field
    /// monitor instead of the empty default `FilterChain` that
    /// `add_subscriber` installs. Update delivery
    /// (`Self::notify_subscribers` / [`Self::post_alarm`]) already
    /// applies `sub.filters`; this is the missing wiring that populates
    /// it.
    ///
    /// The caller passes a FRESH chain per subscriber so stateful
    /// filters (`dbnd` last-value, `dec` counter, `sync` state) stay
    /// isolated across subscribers. An empty chain is a no-op (keeps the
    /// default). No-op when no subscriber matches `sid` (e.g. it was
    /// reaped between add and attach).
    pub fn attach_filters_to_subscriber(
        &self,
        sid: u32,
        filters: crate::server::database::filters::FilterChain,
    ) {
        if filters.is_empty() {
            return;
        }
        let mut subs = self.subscribers.lock();
        if let Some(sub) = subs.iter_mut().find(|s| s.sid == sid) {
            sub.filters = filters;
        }
    }

    /// Remove a subscriber by subscription ID.
    pub fn remove_subscriber(&self, sid: u32) {
        let mut subs = self.subscribers.lock();
        subs.retain(|s| s.sid != sid);
    }
}

/// Subscriber-id source for in-process [`PvSubscription`] monitors on a
/// [`ProcessVariable`]. A `ProcessVariable`'s subscriber `Vec` is disjoint
/// from any `RecordInstance`'s, so this is independent of the record-side
/// allocator; it only has to stay unique among the simple-PV subscribers
/// competing for one PV. Seeded at 1_000_000 for the same reason the
/// record allocator is — keep in-process sids clear of the low,
/// client-assigned wire subscription ids the CA server also registers on
/// the same PV.
static NEXT_PV_SUB_SID: AtomicU32 = AtomicU32::new(1_000_000);

fn next_pv_sub_sid() -> u32 {
    NEXT_PV_SUB_SID.fetch_add(1, Ordering::Relaxed)
}

/// In-process value-change monitor on a simple [`ProcessVariable`], the
/// counterpart of the record-side `DbSubscription`.
///
/// The PUT path (`ProcessVariable::set` / `set_snapshot`) calls
/// `notify_subscribers`, which fans the new value out to every registered
/// subscriber, so a consumer holding a `PvSubscription` observes every
/// later PUT — not just the connect-time snapshot. This mirrors pvxs
/// `SharedPV::post()` delivering a cloned update to each stored subscriber
/// (`sharedpv.cpp:417-440`).
///
/// The handle owns its `Subscriber` slot: `Drop` removes it, so a dropped
/// consumer cannot leave a dead subscriber row in
/// `ProcessVariable.subscribers` — the same leak `DbSubscription`'s `Drop`
/// closes for records.
pub struct PvSubscription {
    reader: EventReader,
    pv: Arc<ProcessVariable>,
    sid: u32,
}

impl PvSubscription {
    /// Register a value-change monitor on `pv`. Returns `None` when the
    /// per-PV subscriber cap is reached. The caller emits the initial
    /// snapshot itself (pvxs `SharedPV::attach` posts the current value
    /// before storing the subscriber); registering the subscriber *before*
    /// reading that snapshot is the miss-free ordering — a PUT racing the
    /// two is then delivered through the stream rather than lost.
    pub async fn subscribe(pv: Arc<ProcessVariable>) -> Option<Self> {
        use crate::server::recgbl::EventMask;
        // VALUE|LOG matches the record-side `DbSubscription` default so
        // simple-PV and record-backed monitors gate identically; a
        // pure-alarm `post_alarm` (ALARM|LOG) still intersects via LOG.
        let mask = (EventMask::VALUE | EventMask::LOG).bits();
        let sid = next_pv_sub_sid();
        // `data_type` is nominal for snapshot consumers: `deliver` ships
        // the full `Snapshot` and gates only on mask/filters, never on the
        // stored type — `DbSubscription` likewise registers as `Double`.
        let reader = pv.add_subscriber(sid, DbFieldType::Double, mask)?;
        Some(Self { reader, pv, sid })
    }

    /// Await the next value change as a full `Snapshot`. A consumer that falls
    /// behind sees the same thing a C monitor does: its earlier distinct queued
    /// updates, and then — once the queue ran short of room — a tail entry
    /// carrying the latest value, because further posts replaced that entry in
    /// place rather than appending (`db_queue_event_log`, `dbEvent.c:812-820`).
    pub async fn recv_snapshot(&mut self) -> Option<Snapshot> {
        Some(self.reader.recv().await?.snapshot)
    }

    /// Non-blocking [`Self::recv_snapshot`]. Delegates to
    /// [`EventReader::try_recv`] (`event_queue.rs:570`) — same queue, same
    /// EVENTS_OFF gate, no suspension.
    ///
    /// Lets a PVA monitor source that adapts this stream be polled from a
    /// blocking drain loop with no reactor present
    /// (`doc/rtems-runtime-portability-design.md` §9 phase 6).
    pub fn try_recv_snapshot(&mut self) -> Result<Snapshot, TryRecvError> {
        self.reader.try_recv().map(|e| e.snapshot)
    }

    /// Await the next change as the full [`MonitorEvent`] — snapshot plus the
    /// per-event `DBE_*` mask. The mask-carrying counterpart of
    /// [`recv_snapshot`](Self::recv_snapshot), matching
    /// `DbSubscription::recv_event` so a consumer can treat a simple-PV and a
    /// record subscription through one shape.
    pub async fn recv_event(&mut self) -> Option<MonitorEvent> {
        self.reader.recv().await
    }

    /// Non-blocking [`Self::recv_event`].
    pub fn try_recv_event(&mut self) -> Result<MonitorEvent, TryRecvError> {
        self.reader.try_recv()
    }
}

impl Drop for PvSubscription {
    fn drop(&mut self) {
        let pv = self.pv.clone();
        let sid = self.sid;
        // Mirror `DbSubscription::drop`: `remove_subscriber` needs an async
        // lock, so remove the slot off-thread. No current runtime means no
        // live subscription to clean up.
        if tokio::runtime::Handle::try_current().is_ok() {
            crate::runtime::task::spawn(async move {
                pv.remove_subscriber(sid);
            });
        }
    }
}

#[cfg(test)]
mod mask_gate_tests {
    use super::*;

    // CA DBE_* monitor mask bits (db_access.h).
    const DBE_VALUE: u16 = 1;
    const DBE_LOG: u16 = 2;
    const DBE_ALARM: u16 = 4;

    fn pv() -> ProcessVariable {
        ProcessVariable::new("test:pv".into(), EpicsValue::Double(0.0))
    }

    /// A full-snapshot write must persist alarm + timestamp + userTag so
    /// a later `snapshot()` (the GET path) reflects them — not just the
    /// live monitor fan-out. A subsequent value-only `set()` carries no
    /// explicit metadata and must revert the snapshot to NO_ALARM.
    #[epics_macros_rs::epics_test]
    async fn set_snapshot_metadata_persists_then_value_set_clears() {
        let pv = pv();

        // 42 ns exact: a `SystemTime` rounds this to 0 on Windows, so the
        // round-trip is built from `WallTime` integers to actually exercise
        // sub-100 ns persistence through `PostedMeta`.
        let posted_time = WallTime::from_unix(1_600_000_000, 42);
        let mut snap = Snapshot::new(EpicsValue::Double(7.0), 3, 2, posted_time);
        snap.user_tag = 9;
        pv.set_snapshot(snap);

        let got = pv.snapshot();
        assert_eq!(got.value, EpicsValue::Double(7.0), "value persisted");
        assert_eq!(got.alarm.status, 3, "alarm.status persisted to GET");
        assert_eq!(got.alarm.severity, 2, "alarm.severity persisted to GET");
        assert_eq!(got.user_tag, 9, "userTag persisted to GET");
        assert_eq!(got.timestamp, posted_time, "timestamp persisted to GET");

        // A plain value write reverts to the bare-PV default.
        pv.set(EpicsValue::Double(8.0));
        let after = pv.snapshot();
        assert_eq!(after.value, EpicsValue::Double(8.0));
        assert_eq!(after.alarm.status, 0, "value set clears posted alarm");
        assert_eq!(after.alarm.severity, 0, "value set clears posted severity");
        assert_eq!(after.user_tag, 0, "value set clears posted userTag");
        assert_ne!(
            after.timestamp, posted_time,
            "value set must restamp the timestamp, not keep the posted one"
        );
    }

    /// a `DBE_ALARM`-only subscriber must not receive a plain
    /// value set, but must receive an alarm post.
    #[epics_macros_rs::epics_test]
    async fn alarm_only_subscriber_skips_value_post() {
        let pv = pv();
        let mut rx = pv
            .add_subscriber(1, DbFieldType::Double, DBE_ALARM)
            .expect("subscriber added");
        pv.set(EpicsValue::Double(1.0));
        assert!(
            rx.try_recv().is_err(),
            "DBE_ALARM-only subscriber must not receive a value post"
        );
        pv.post_alarm(2, 3);
        assert!(
            rx.try_recv().is_ok(),
            "DBE_ALARM subscriber must receive an alarm post"
        );
    }

    /// a `DBE_VALUE`-only subscriber must not receive a
    /// `post_alarm`, but must receive value sets.
    #[epics_macros_rs::epics_test]
    async fn value_only_subscriber_skips_alarm_post() {
        let pv = pv();
        let mut rx = pv
            .add_subscriber(1, DbFieldType::Double, DBE_VALUE)
            .expect("subscriber added");
        pv.post_alarm(2, 3);
        assert!(
            rx.try_recv().is_err(),
            "DBE_VALUE-only subscriber must not receive an alarm post"
        );
        pv.set(EpicsValue::Double(1.0));
        assert!(
            rx.try_recv().is_ok(),
            "DBE_VALUE subscriber must receive a value post"
        );
    }

    // --- Regression: set_snapshot must reach DBE_LOG and DBE_ALARM-only subs ---

    fn snapshot() -> Snapshot {
        Snapshot::new(
            EpicsValue::Double(2.0),
            0,
            0,
            std::time::SystemTime::UNIX_EPOCH,
        )
    }

    /// A DBE_LOG (archiver) subscriber must receive a set_snapshot post.
    #[epics_macros_rs::epics_test]
    async fn log_subscriber_receives_snapshot_post() {
        let pv = pv();
        let mut rx = pv
            .add_subscriber(1, DbFieldType::Double, DBE_LOG)
            .expect("subscriber added");
        pv.set_snapshot(snapshot());
        assert!(
            rx.try_recv().is_ok(),
            "DBE_LOG subscriber must receive a set_snapshot post"
        );
    }

    /// A DBE_ALARM-only subscriber must receive a set_snapshot post.
    #[epics_macros_rs::epics_test]
    async fn alarm_only_subscriber_receives_snapshot_post() {
        let pv = pv();
        let mut rx = pv
            .add_subscriber(1, DbFieldType::Double, DBE_ALARM)
            .expect("subscriber added");
        pv.set_snapshot(snapshot());
        assert!(
            rx.try_recv().is_ok(),
            "DBE_ALARM-only subscriber must receive a set_snapshot post"
        );
    }

    /// A DBE_VALUE subscriber must still receive a set_snapshot post.
    #[epics_macros_rs::epics_test]
    async fn value_subscriber_receives_snapshot_post() {
        let pv = pv();
        let mut rx = pv
            .add_subscriber(1, DbFieldType::Double, DBE_VALUE)
            .expect("subscriber added");
        pv.set_snapshot(snapshot());
        assert!(
            rx.try_recv().is_ok(),
            "DBE_VALUE subscriber must receive a set_snapshot post"
        );
    }

    /// A `DBE_VALUE | DBE_ALARM` subscriber receives both event classes.
    #[epics_macros_rs::epics_test]
    async fn both_classes_receive_both_posts() {
        let pv = pv();
        let mut rx = pv
            .add_subscriber(1, DbFieldType::Double, DBE_VALUE | DBE_ALARM)
            .expect("subscriber added");
        pv.set(EpicsValue::Double(1.0));
        assert!(rx.try_recv().is_ok(), "value post delivered to VALUE|ALARM");
        pv.post_alarm(2, 3);
        assert!(rx.try_recv().is_ok(), "alarm post delivered to VALUE|ALARM");
    }

    /// A DBE_LOG-only subscriber (archiver) must receive both value
    /// events and alarm events.  Pre-fix: VALUE-only / ALARM-only post masks
    /// never intersected DBE_LOG(2), so archivers received silence.
    #[epics_macros_rs::epics_test]
    async fn br_r52_log_subscriber_receives_value_and_alarm_events() {
        const DBE_LOG: u16 = 2;
        let pv = pv();
        let mut rx = pv
            .add_subscriber(1, DbFieldType::Double, DBE_LOG)
            .expect("subscriber added");
        pv.set(EpicsValue::Double(1.0));
        assert!(
            rx.try_recv().is_ok(),
            "DBE_LOG subscriber must receive a value post"
        );
        pv.post_alarm(2, 3);
        assert!(
            rx.try_recv().is_ok(),
            "DBE_LOG subscriber must receive an alarm post"
        );
    }

    /// Every delivered event carries its post's `DBE_*` class — the
    /// per-event mask C attaches to the field log (`db_field_log.mask`)
    /// and pvxs narrows monitor decoding with (`groupsource.cpp:331-337`).
    #[epics_macros_rs::epics_test]
    async fn monitor_event_carries_post_class_mask() {
        use crate::server::recgbl::EventMask;
        let pv = pv();
        let mut rx = pv
            .add_subscriber(1, DbFieldType::Double, DBE_VALUE | DBE_LOG | DBE_ALARM)
            .expect("subscriber added");
        pv.set(EpicsValue::Double(1.0));
        assert_eq!(
            rx.try_recv().expect("value event").mask,
            EventMask::VALUE | EventMask::LOG,
            "value post carries VALUE|LOG"
        );
        pv.post_alarm(2, 3);
        assert_eq!(
            rx.try_recv().expect("alarm event").mask,
            EventMask::ALARM | EventMask::LOG,
            "alarm post carries ALARM|LOG"
        );
    }

    /// When the queue runs short of room and a post replaces this monitor's
    /// last entry in place, the surviving entry's mask is the OR of the
    /// displaced event's class and its own: the displaced *value* is gone (C
    /// frees the field log), but a narrow consumer must still learn that an
    /// ALARM-class change happened inside the coalesced tail.
    #[epics_macros_rs::epics_test]
    async fn in_place_replacement_accumulates_event_class_masks() {
        use crate::server::event_queue::{event_que_size, events_per_que};
        use crate::server::recgbl::EventMask;
        let pv = Arc::new(ProcessVariable::new(
            "coalesce:mask".into(),
            EpicsValue::Double(0.0),
        ));
        let mut reader = pv
            .add_subscriber(7, DbFieldType::Double, DBE_VALUE | DBE_LOG | DBE_ALARM)
            .expect("subscriber added");
        // Append VALUE|LOG posts until the ring space reaches the replace
        // threshold; from here every post overwrites the tail entry.
        let appended = event_que_size() - events_per_que();
        for i in 1..=appended {
            pv.set(EpicsValue::Double(i as f64));
        }
        // Replaces the tail: its class (ALARM|LOG) must not be lost.
        pv.post_alarm(2, 3);
        // Replaces it again with a value post — both classes fold into the
        // survivor.
        pv.set(EpicsValue::Double(99.0));

        let mut last = None;
        while let Ok(event) = reader.try_recv() {
            last = Some(event);
        }
        let delivered = last.expect("the tail entry is delivered");
        assert_eq!(
            delivered.snapshot.value.to_f64(),
            Some(99.0),
            "the tail entry carries the newest value"
        );
        assert!(
            delivered
                .mask
                .contains(EventMask::VALUE | EventMask::ALARM | EventMask::LOG),
            "the displaced alarm class survives in the delivered mask (got {:?})",
            delivered.mask
        );
    }

    /// R8-22 (simple-PV path): a monitor whose queue runs out of room during a
    /// burst must receive its EARLIER DISTINCT queued updates and then a tail
    /// entry carrying the latest value — C `db_queue_event_log` replaces only
    /// `*pLastLog` (`dbEvent.c:812-820`) and leaves the earlier entries queued.
    ///
    /// The old primitive parked the newest value in a side coalesce slot, and
    /// the consumer, finding it set, discarded the ENTIRE queued backlog and
    /// delivered only that newest value — so a 200-post burst came out as a
    /// single event instead of {1..107, 200}.
    #[epics_macros_rs::epics_test]
    async fn r8_22_pv_burst_keeps_earlier_distinct_updates() {
        use crate::server::event_queue::{event_que_size, events_per_que};
        use std::time::Duration;
        let pv = Arc::new(ProcessVariable::new(
            "coalesce:pv".into(),
            EpicsValue::Double(0.0),
        ));
        let mut sub = PvSubscription::subscribe(pv.clone())
            .await
            .expect("subscribe");
        // With nothing draining, the first `appended` posts take ring entries
        // and every later post replaces the tail entry in place.
        let appended = event_que_size() - events_per_que();
        let burst = appended + 92;
        for i in 1..=burst {
            pv.set(EpicsValue::Double(i as f64));
        }
        let mut seq = Vec::new();
        while let Ok(Some(snap)) =
            crate::runtime::task::timeout(Duration::from_millis(200), sub.recv_snapshot()).await
        {
            seq.push(snap.value.to_f64().expect("double value"));
        }
        let want: Vec<f64> = (1..appended)
            .map(|i| i as f64)
            .chain(std::iter::once(burst as f64))
            .collect();
        assert_eq!(
            seq, want,
            "burst delivery must be {{earlier distinct backlog…, coalesced tail}}"
        );
    }
}

#[cfg(test)]
mod metadata_tests {
    use super::*;

    fn meta() -> PvMetadata {
        PvMetadata {
            display: Some(DisplayInfo {
                units: "degC".into(),
                precision: 2,
                upper_disp_limit: 100.0,
                lower_disp_limit: -50.0,
                upper_alarm_limit: 90.0,
                upper_warning_limit: 80.0,
                lower_warning_limit: -20.0,
                lower_alarm_limit: -40.0,
                ..Default::default()
            }),
            control: Some(ControlInfo {
                upper_ctrl_limit: 95.0,
                lower_ctrl_limit: -45.0,
            }),
            enums: None,
        }
    }

    fn pv() -> ProcessVariable {
        ProcessVariable::new("m:pv".into(), EpicsValue::Double(1.0))
    }

    /// `set_with_origin` tags the value event with the writer's origin,
    /// plain `set` stays untagged, and a plain `set` inside an
    /// `AmbientWriteOriginScope` inherits the scope's origin — the
    /// simple-PV side of the record funnels' inheritance rule.
    #[epics_macros_rs::epics_test]
    async fn set_with_origin_tags_the_value_event() {
        const DBE_VALUE: u16 = 1;
        let pv = pv();
        let mut rx = pv
            .add_subscriber(1, DbFieldType::Double, DBE_VALUE)
            .expect("subscriber added");

        pv.set(EpicsValue::Double(2.0));
        assert_eq!(rx.try_recv().expect("plain set posts").origin, 0);

        pv.set_with_origin(EpicsValue::Double(3.0), 77);
        assert_eq!(rx.try_recv().expect("tagged set posts").origin, 77);

        {
            let _scope = crate::server::record::ambient_write_origin_scope(88);
            pv.set(EpicsValue::Double(4.0));
        }
        assert_eq!(
            rx.try_recv().expect("ambient-scoped set posts").origin,
            88,
            "an originless simple-PV post inside an ambient scope must inherit it"
        );
    }

    /// A bare PV serves no metadata until a proxy installs it; after
    /// `set_metadata`, the GET snapshot carries the shadow DBR_GR/DBR_CTRL.
    #[epics_macros_rs::epics_test]
    async fn set_metadata_serves_on_get_snapshot() {
        let pv = pv();
        assert!(
            pv.snapshot().display.is_none(),
            "bare PV must carry no metadata before install"
        );
        pv.set_metadata(meta());
        let snap = pv.snapshot();
        let d = snap.display.expect("display installed");
        assert_eq!(d.units, "degC");
        assert_eq!(d.precision, 2);
        assert_eq!(
            snap.control.expect("control installed").upper_ctrl_limit,
            95.0
        );
    }

    /// A CTRL-type monitor must see the installed limits on every value
    /// event, not only the initial GET — value posts carry the metadata.
    #[epics_macros_rs::epics_test]
    async fn installed_metadata_rides_value_posts() {
        const DBE_VALUE: u16 = 1;
        let pv = pv();
        pv.set_metadata(meta());
        let mut rx = pv
            .add_subscriber(1, DbFieldType::Double, DBE_VALUE)
            .expect("subscriber added");
        pv.set(EpicsValue::Double(2.0));
        let ev = rx.try_recv().expect("value event delivered");
        assert_eq!(
            ev.snapshot.display.expect("metadata on value post").units,
            "degC"
        );
    }

    /// `apply_metadata` only supplies fields the caller left absent: a
    /// gateway snapshot that already carries its own display wins.
    #[epics_macros_rs::epics_test]
    async fn apply_metadata_does_not_clobber_caller_metadata() {
        const DBE_VALUE: u16 = 1;
        let pv = pv();
        pv.set_metadata(meta()); // installed units = degC
        let mut rx = pv
            .add_subscriber(1, DbFieldType::Double, DBE_VALUE)
            .expect("subscriber added");
        let mut snap = Snapshot::new(
            EpicsValue::Double(3.0),
            0,
            0,
            std::time::SystemTime::UNIX_EPOCH,
        );
        snap.display = Some(DisplayInfo {
            units: "volts".into(),
            ..Default::default()
        });
        pv.set_snapshot(snap);
        let ev = rx.try_recv().expect("snapshot delivered");
        assert_eq!(
            ev.snapshot.display.expect("caller display kept").units,
            "volts"
        );
    }

    /// `post_property` reaches DBE_PROPERTY subscribers (carrying the
    /// metadata) and not DBE_VALUE-only subscribers.
    #[epics_macros_rs::epics_test]
    async fn post_property_reaches_only_property_subscribers() {
        const DBE_VALUE: u16 = 1;
        const DBE_PROPERTY: u16 = 8;
        let pv = pv();
        pv.set_metadata(meta());
        let mut prop_rx = pv
            .add_subscriber(1, DbFieldType::Double, DBE_PROPERTY)
            .expect("subscriber added");
        let mut val_rx = pv
            .add_subscriber(2, DbFieldType::Double, DBE_VALUE)
            .expect("subscriber added");
        pv.post_property(Snapshot::new(
            EpicsValue::Double(1.0),
            0,
            0,
            std::time::SystemTime::UNIX_EPOCH,
        ))
        .await;
        let ev = prop_rx
            .try_recv()
            .expect("DBE_PROPERTY subscriber receives property post");
        assert_eq!(
            ev.snapshot
                .display
                .expect("property post carries metadata")
                .units,
            "degC"
        );
        assert!(
            val_rx.try_recv().is_err(),
            "DBE_VALUE-only subscriber must not receive a property post"
        );
    }

    /// A property post
    /// must carry the upstream CTRL event's status/severity and timestamp,
    /// not a fabricated `NO_ALARM` / wall-clock-now snapshot. C ca-gateway
    /// preserves `setStatSevr()` on the property callback
    /// (`gatePv.cc:2413-2438`); a downstream `DBE_PROPERTY` monitor must
    /// see `severity=MAJOR` and the upstream timestamp, even though only
    /// metadata changed.
    #[epics_macros_rs::epics_test]
    async fn post_property_preserves_upstream_alarm_and_timestamp() {
        const DBE_PROPERTY: u16 = 8;
        const MAJOR: u16 = 2; // epicsSevMajor
        const HIGH: u16 = 3; // epicsAlarmHigh
        let pv = pv();
        pv.set_metadata(meta());
        let mut prop_rx = pv
            .add_subscriber(1, DbFieldType::Double, DBE_PROPERTY)
            .expect("subscriber added");
        // The upstream CTRL event timestamp: a fixed point in the past, so
        // it is unmistakably NOT a fresh wall clock minted by the post.
        let upstream_ts = WallTime::from_unix(1_000_000, 0);
        pv.post_property(Snapshot::new(
            EpicsValue::Double(2.0),
            HIGH,
            MAJOR,
            upstream_ts,
        ))
        .await;
        let ev = prop_rx.try_recv().expect("property post delivered");
        assert_eq!(
            ev.snapshot.alarm.severity, MAJOR,
            "property post must carry the upstream MAJOR severity, not NO_ALARM"
        );
        assert_eq!(ev.snapshot.alarm.status, HIGH, "upstream status preserved");
        assert_eq!(
            ev.snapshot.timestamp, upstream_ts,
            "property post must keep the upstream timestamp, not a fresh wall clock"
        );
        // Shadow metadata is still overlaid onto the upstream snapshot.
        assert_eq!(
            ev.snapshot
                .display
                .expect("property post carries shadow metadata")
                .units,
            "degC"
        );
    }
}

#[cfg(test)]
mod read_hook_tests {
    use super::*;

    fn pv() -> ProcessVariable {
        ProcessVariable::new("g:pv".into(), EpicsValue::Double(1.0))
    }

    /// No hook installed (the default for every record-backed and cached
    /// PV): `read_snapshot` is exactly `snapshot` wrapped in `Ok` — the
    /// stored value, byte-for-byte unchanged.
    #[epics_macros_rs::epics_test]
    async fn read_snapshot_without_hook_equals_snapshot() {
        let pv = pv();
        let read = pv.read_snapshot().await.expect("no-hook read never errors");
        let stored = pv.snapshot();
        assert_eq!(read.value, stored.value);
        assert_eq!(read.value, EpicsValue::Double(1.0));
    }

    /// With a hook installed (no-cache mode), the GET value comes fresh
    /// from the hook, NOT from the stored shadow value — the stored value
    /// stays a stale sentinel that the hook overrides.
    #[epics_macros_rs::epics_test]
    async fn read_snapshot_fires_hook_for_fresh_value() {
        let pv = pv();
        // Stored shadow value is a sentinel the hook must override.
        pv.set(EpicsValue::Double(999.0));
        pv.set_read_hook(Arc::new(|| {
            Box::pin(async {
                Ok(Snapshot::new(
                    EpicsValue::Double(42.0),
                    0,
                    0,
                    std::time::UNIX_EPOCH,
                ))
            })
        }));
        let read = pv.read_snapshot().await.expect("hook returns Ok");
        assert_eq!(
            read.value,
            EpicsValue::Double(42.0),
            "GET must serve the hook's fresh value, not the stored sentinel"
        );
    }

    /// A hook failure propagates so the server can answer `ECA_GETFAIL`,
    /// matching C ca-gateway forwarding each read to the IOC.
    #[epics_macros_rs::epics_test]
    async fn read_snapshot_propagates_hook_error() {
        let pv = pv();
        pv.set_read_hook(Arc::new(|| Box::pin(async { Err(CaError::Disconnected) })));
        let err = pv.read_snapshot().await.expect_err("hook error propagates");
        assert!(matches!(err, CaError::Disconnected));
    }

    /// No hook (every record-backed and cached PV): the sync companion
    /// `read_snapshot_local` yields `Some(snapshot)`, byte-for-byte the same
    /// value as `snapshot` / the async `read_snapshot` — the fully sans-io
    /// GET path.
    #[test]
    fn read_snapshot_local_without_hook_is_some_and_matches_snapshot() {
        let pv = pv();
        let local = pv
            .read_snapshot_local()
            .expect("no hook ⇒ sync snapshot is Some");
        assert_eq!(local.value, pv.snapshot().value);
        assert_eq!(local.value, EpicsValue::Double(1.0));
    }

    /// A read hook installed (gateway no-cache): the sync companion returns
    /// `None`, the signal that the caller must take the async upstream-GET
    /// path — `read_snapshot_local` never fires the hook itself.
    #[test]
    fn read_snapshot_local_with_hook_is_none() {
        let pv = pv();
        pv.set_read_hook(Arc::new(|| {
            Box::pin(async {
                Ok(Snapshot::new(
                    EpicsValue::Double(42.0),
                    0,
                    0,
                    std::time::UNIX_EPOCH,
                ))
            })
        }));
        assert!(
            pv.read_snapshot_local().is_none(),
            "a read hook ⇒ the sync path defers to the async upstream GET"
        );
    }

    /// The read hook is GET-path only: `snapshot` (monitor fan-out, the
    /// initial monitor event, access-rights re-posts) keeps serving the
    /// stored value even when a hook is installed.
    #[epics_macros_rs::epics_test]
    async fn snapshot_ignores_read_hook() {
        let pv = pv();
        pv.set(EpicsValue::Double(7.0));
        pv.set_read_hook(Arc::new(|| {
            Box::pin(async {
                Ok(Snapshot::new(
                    EpicsValue::Double(42.0),
                    0,
                    0,
                    std::time::UNIX_EPOCH,
                ))
            })
        }));
        let snap = pv.snapshot();
        assert_eq!(
            snap.value,
            EpicsValue::Double(7.0),
            "snapshot must serve the stored value, never the read hook"
        );
    }

    /// Fresh value + upstream alarm/time ride from the hook; the shadow's
    /// installed *property* metadata (display/control/enum) — which a
    /// `DBR_TIME_*` event does not carry — is overlaid for those fields.
    #[epics_macros_rs::epics_test]
    async fn read_snapshot_carries_shadow_metadata() {
        let pv = pv();
        pv.set_metadata(PvMetadata {
            display: Some(DisplayInfo {
                units: "mm".into(),
                precision: 3,
                ..Default::default()
            }),
            control: None,
            enums: None,
        });
        // The hook returns a Time-class snapshot (value + alarm + time,
        // no display/control/enum), exactly as `get_with_metadata(Time)`.
        pv.set_read_hook(Arc::new(|| {
            Box::pin(async {
                Ok(Snapshot::new(
                    EpicsValue::Double(5.0),
                    0,
                    0,
                    std::time::UNIX_EPOCH,
                ))
            })
        }));
        let read = pv.read_snapshot().await.expect("hook returns Ok");
        assert_eq!(read.value, EpicsValue::Double(5.0));
        assert_eq!(
            read.display
                .expect("shadow property metadata rides fresh value")
                .units,
            "mm"
        );
    }

    /// A no-cache GET must report the FRESH upstream alarm and timestamp
    /// that travel with the value (C `getTimeCB` decodes the `DBR_TIME_*`
    /// event's status/severity/time before `setEventData`,
    /// `gatePv.cc:1789-1794`), NOT the shadow's last monitor-posted (or
    /// bare-PV default) alarm/time. Before the fix the read hook returned
    /// a bare value and `read_snapshot` grafted it onto the stored
    /// snapshot, so the GET reported the new value with a stale or default
    /// status/severity/timestamp.
    #[epics_macros_rs::epics_test]
    async fn read_snapshot_carries_upstream_alarm_not_shadow() {
        use std::time::{Duration, UNIX_EPOCH};
        let pv = pv();
        // The shadow's stored snapshot carries one alarm/time (a prior
        // monitor post). Make it concrete and DIFFERENT from the upstream
        // GET so a graft-onto-shadow regression is observable.
        let shadow_time = UNIX_EPOCH + Duration::from_secs(1_000);
        pv.set_snapshot(Snapshot::new(EpicsValue::Double(1.0), 7, 1, shadow_time));
        // The fresh upstream GET reports a different value, alarm, and time.
        let upstream_time = WallTime::from_unix(2_000, 0);
        pv.set_read_hook(Arc::new(move || {
            Box::pin(
                async move { Ok(Snapshot::new(EpicsValue::Double(5.0), 17, 2, upstream_time)) },
            )
        }));
        let read = pv.read_snapshot().await.expect("hook returns Ok");
        assert_eq!(read.value, EpicsValue::Double(5.0), "fresh upstream value");
        assert_eq!(
            read.alarm.status, 17,
            "upstream alarm status, not shadow's 7"
        );
        assert_eq!(read.alarm.severity, 2, "upstream severity, not shadow's 1");
        assert_eq!(
            read.timestamp, upstream_time,
            "upstream timestamp, not shadow's"
        );
    }
}
