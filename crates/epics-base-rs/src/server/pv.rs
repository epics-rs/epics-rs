use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::runtime::sync::{Mutex, RwLock, mpsc};

use crate::error::CaError;
use crate::server::snapshot::Snapshot;
use crate::types::{DbFieldType, EpicsValue};

/// Per-subscriber bounded mpsc depth. Lifted from
/// `EPICS_CAS_MAX_EVENTS_PER_CHAN`, default 64. Floor 4 so even
/// hostile env values (`0`/`1`) leave room for last-value
/// coalescing to make progress.
fn per_channel_event_depth() -> usize {
    crate::runtime::env::get("EPICS_CAS_MAX_EVENTS_PER_CHAN")
        .and_then(|s| s.parse::<usize>().ok())
        .unwrap_or(64)
        .max(4)
}

/// Per-PV subscriber cap (P-G14). Default 1024 — comfortably above
/// any realistic dashboard fan-out, small enough to bound the
/// per-PV `Vec<Subscriber>` under abuse. Override via
/// `EPICS_CAS_MAX_SUBSCRIBERS_PER_PV`.
pub(crate) fn max_subscribers_per_pv() -> usize {
    crate::runtime::env::get("EPICS_CAS_MAX_SUBSCRIBERS_PER_PV")
        .and_then(|s| s.parse::<usize>().ok())
        .unwrap_or(1024)
        .max(8)
}

/// Process-global counter of monitor events dropped because the
/// per-channel mpsc was full AND the coalesce slot was already
/// occupied by an even-newer overflow value. Covers both
/// `ProcessVariable` and `RecordInstance` overflow via the single
/// [`Subscriber::coalesce_overflow`] owner. Mirrors the pattern of
/// `dropped_monitors` on the client side (subscribe_with_deadband).
///
/// BFR-10: read via [`dropped_monitor_events`]. That reader is not yet
/// wired to a live scrape surface — the `/queues` admin endpoint
/// currently renders configured limits only, not this counter — so do
/// not assume the value is observable through an endpoint until that
/// wiring lands.
static DROPPED_MONITOR_EVENTS: AtomicU64 = AtomicU64::new(0);

/// Read the cumulative count of dropped monitor events. Intended for
/// introspection / metrics; see [`DROPPED_MONITOR_EVENTS`] for the
/// current wiring status.
pub fn dropped_monitor_events() -> u64 {
    DROPPED_MONITOR_EVENTS.load(Ordering::Relaxed)
}

/// Internal: record a dropped event. Called from
/// `notify_subscribers` when both the bounded mpsc and the
/// coalesce slot are full.
fn record_dropped_monitor() {
    DROPPED_MONITOR_EVENTS.fetch_add(1, Ordering::Relaxed);
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

/// BRIDGE-FR-1: read/write access decision for a gateway shadow PV,
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

/// BRIDGE-FR-1: per-PV access hook installed by a proxy (the CA
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

/// A monitor event sent to subscribers when a PV value changes.
/// Carries a full Snapshot so GR/CTRL metadata (PREC, EGU, limits) is available.
#[derive(Debug, Clone)]
pub struct MonitorEvent {
    pub snapshot: Snapshot,
    /// Origin writer ID. When non-zero, subscribers with the same
    /// `ignore_origin` can filter out self-triggered events.
    /// Used to prevent sequencer write-back loops.
    ///
    /// **Scope**: Currently tagged on `put_pv_and_post_with_origin` events only.
    /// Events from `process_record_with_links` (process path) always have
    /// origin=0. If a future sequencer needs to filter process-path events
    /// too, origin tagging can be extended to the process path by passing
    /// origin through `ProcessOutcome` or `process_record_with_links`.
    pub origin: u64,
}

/// A subscriber waiting for PV value updates.
pub struct Subscriber {
    pub sid: u32,
    pub data_type: DbFieldType,
    pub mask: u16,
    pub tx: mpsc::Sender<MonitorEvent>,
    /// Last-value coalescing slot. When the bounded mpsc above is full,
    /// the producer stores the newest event here, overwriting any prior
    /// pending overflow value. The consumer drains this after each normal
    /// recv() to deliver the most recent state — matching libca rsrv
    /// "drop oldest, keep newest" semantics.
    pub coalesced: Arc<StdMutex<Option<MonitorEvent>>>,
    /// Server-side channel filter chain (epics-base 3.15.7).
    /// Defaults to empty — every event passes unchanged. Populated
    /// by the subscription path when the channel name carries a
    /// `.{filter:opts}` JSON suffix (`dbnd`, `arr`, `ts`, ...).
    pub filters: crate::server::database::filters::FilterChain,
}

impl Subscriber {
    /// CA-FR-6: a monitor delivery is gated on the requested `DBE_*`
    /// mask. Returns true only when the post's event class intersects
    /// this subscriber's mask — the single rule C rsrv enforces with
    /// `caEventMask & pevent->select` (`dbEvent.c:892-900`) and the
    /// same intersection the record-field monitor path applies. An
    /// empty post class (no specific class) delivers unconditionally.
    fn accepts(&self, post: crate::server::recgbl::EventMask) -> bool {
        post.is_empty() || crate::server::recgbl::EventMask::from_bits(self.mask).intersects(post)
    }

    /// BFR-10: single owner of the slow-consumer coalesce overflow.
    /// Call after the bounded `tx` rejected a send: store the newest
    /// `event` in the coalesce slot, and — when it displaces a value the
    /// consumer never observed — record one dropped monitor event.
    ///
    /// Every coalesced-slot overwrite on the overflow path (both
    /// `ProcessVariable` value/alarm posts and `RecordInstance`
    /// field monitors) MUST go through here, so `dropped_monitor_events()`
    /// cannot undercount one path: the record-field path previously
    /// overwrote the slot directly and never bumped the counter, hiding
    /// slow-consumer loss for the path most CA/PVA database monitors use.
    pub(crate) fn coalesce_overflow(&self, event: MonitorEvent) {
        if let Ok(mut slot) = self.coalesced.lock() {
            if slot.is_some() {
                record_dropped_monitor();
            }
            *slot = Some(event);
        }
    }
}

/// A process variable hosted by the server.
pub struct ProcessVariable {
    pub name: String,
    pub value: RwLock<EpicsValue>,
    pub subscribers: Mutex<Vec<Subscriber>>,
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
    /// BRIDGE-FR-1: optional access hook consulted by the CA server's
    /// `compute_access` to decide a downstream client's read/write
    /// rights for this PV. When set, it overrides the server's own ACF
    /// for this PV — the gateway uses it to enforce `.pvlist` ASG-based
    /// `can_read` / `can_write`, symmetric to [`Self::write_hook`].
    /// Same sync `parking_lot::RwLock` slot rationale as `write_hook`.
    access_hook: parking_lot::RwLock<Option<AccessHook>>,
}

impl ProcessVariable {
    pub fn new(name: String, initial: EpicsValue) -> Self {
        Self {
            name,
            value: RwLock::new(initial),
            subscribers: Mutex::new(Vec::new()),
            write_hook: parking_lot::RwLock::new(None),
            access_hook: parking_lot::RwLock::new(None),
        }
    }

    /// Install an access hook (BRIDGE-FR-1). Replaces any previously
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
    pub async fn get(&self) -> EpicsValue {
        self.value.read().await.clone()
    }

    /// Build a Snapshot for this bare PV.
    ///
    /// A `ProcessVariable` is a non-record-backed channel: it has no
    /// alarm engine, no DESC/EGU/PREC metadata, no timestamp user
    /// tag and no enum/control limits. The snapshot is therefore
    /// deliberately minimal — value + `NO_ALARM` + wall-clock now,
    /// with `display`/`control`/`enums` = `None` and `user_tag` = 0.
    /// These zeros are correct, not placeholder: there is no source
    /// to draw richer metadata from. Record-backed channels build
    /// their snapshot via `RecordInstance::snapshot_for_field`, which
    /// carries the record's alarm/metadata. The only path that
    /// injects a non-zero alarm onto a bare PV is [`Self::post_alarm`]
    /// (used by the CA/PVA gateway adapter to surface upstream
    /// disconnect).
    pub async fn snapshot(&self) -> Snapshot {
        let value = self.value.read().await.clone();
        Snapshot::new(value, 0, 0, crate::runtime::time::now_wall())
    }

    /// Set a new value and notify all subscribers.
    pub async fn set(&self, new_value: EpicsValue) {
        {
            let mut val = self.value.write().await;
            *val = new_value.clone();
        }
        self.notify_subscribers(new_value).await;
    }

    /// Set value from a full snapshot (value + alarm + timestamp) and notify
    /// all subscribers. Used by the CA gateway forwarding task to propagate
    /// the upstream alarm status/severity and IOC timestamp to downstream
    /// monitors. Mirrors `gateVcData::setEventData` + `vcPostEvent` in the
    /// C ca-gateway: the incoming `dbr_time_xxx` GDD carries all three fields.
    pub async fn set_snapshot(&self, snapshot: Snapshot) {
        {
            let mut val = self.value.write().await;
            *val = snapshot.value.clone();
        }
        self.notify_subscribers_from_snapshot(snapshot).await;
    }

    /// Push a fresh monitor event holding the current value but with
    /// the supplied alarm severity/status. Used by the PVA / CA
    /// gateway adapter to surface upstream-disconnect to downstream
    /// monitor subscribers without dropping the simple PV (which
    /// would force every downstream client into ECA_DISCONN +
    /// reconnect storms when the upstream is just briefly
    /// unreachable). Mirrors gatePvData::death's "alarm-post"
    /// alternative discussed in the C++ ca-gateway audit.
    pub async fn post_alarm(&self, severity: u16, status: u16) {
        use crate::server::database::filters::FilteredMonitorEvent;
        use crate::server::recgbl::EventMask;
        let value = self.value.read().await.clone();
        let mut subs = self.subscribers.lock().await;
        subs.retain(|sub| !sub.tx.is_closed());
        // BR-R52: ALARM|LOG so DBE_LOG (archiver) subscribers receive alarm events.
        let post = EventMask::ALARM | EventMask::LOG;
        for sub in subs.iter() {
            // Skip subscribers that did not request alarm events
            // (`caEventMask & pevent->select == 0`).
            if !sub.accepts(post) {
                continue;
            }
            let snapshot = Snapshot::new(
                value.clone(),
                status,
                severity,
                crate::runtime::time::now_wall(),
            );
            let event = MonitorEvent {
                snapshot,
                origin: 0,
            };
            // `FilteredMonitorEvent` with `mask = ALARM` tells
            // value filters (e.g. `dbnd`) to pass through (446e0d4a).
            let filtered = if sub.filters.is_empty() {
                Some(event)
            } else {
                sub.filters
                    .apply(FilteredMonitorEvent::new(event, post))
                    .map(|fe| fe.event)
            };
            let Some(event) = filtered else {
                continue;
            };
            if sub.tx.try_send(event.clone()).is_err() {
                // L4 / BFR-10: an alarm event overwriting an unconsumed
                // coalesced slot is genuinely lost. Route through the
                // single coalesce-overflow owner so alarm-event loss to
                // a slow consumer is counted exactly like value events.
                sub.coalesce_overflow(event);
            }
        }
    }

    /// Notify all subscribers of a new value.
    async fn notify_subscribers(&self, value: EpicsValue) {
        use crate::server::database::filters::FilteredMonitorEvent;
        use crate::server::recgbl::EventMask;
        let mut subs = self.subscribers.lock().await;
        // Remove subscribers whose channel has been dropped
        subs.retain(|sub| !sub.tx.is_closed());
        // BR-R52: VALUE|LOG so DBE_LOG (archiver) subscribers receive value events.
        let post = EventMask::VALUE | EventMask::LOG;
        for sub in subs.iter() {
            // Skip subscribers that did not request value events
            // (e.g. a `DBE_ALARM`-only monitor).
            if !sub.accepts(post) {
                continue;
            }
            let snapshot = Snapshot::new(value.clone(), 0, 0, crate::runtime::time::now_wall());
            let event = MonitorEvent {
                snapshot,
                origin: 0,
            };
            // Value-changed emission — channel filters may suppress
            // this event when (e.g.) the deadband isn't crossed.
            let filtered = if sub.filters.is_empty() {
                Some(event)
            } else {
                sub.filters
                    .apply(FilteredMonitorEvent::new(event, post))
                    .map(|fe| fe.event)
            };
            let Some(event) = filtered else {
                continue;
            };
            if sub.tx.try_send(event.clone()).is_err() {
                // Queue full — overwrite any prior pending overflow with
                // the newest event (consumer picks it up via
                // `pop_coalesced` after the next normal recv). The single
                // coalesce-overflow owner counts a value that the
                // consumer never observed as a dropped monitor event.
                sub.coalesce_overflow(event);
            }
        }
    }

    /// Notify all subscribers using a pre-built Snapshot (value + alarm +
    /// timestamp). Used by `set_snapshot` to propagate the upstream alarm
    /// and IOC timestamp without synthesising a new zero-alarm local-time
    /// snapshot.
    async fn notify_subscribers_from_snapshot(&self, snapshot: Snapshot) {
        use crate::server::database::filters::FilteredMonitorEvent;
        use crate::server::recgbl::EventMask;
        let mut subs = self.subscribers.lock().await;
        subs.retain(|sub| !sub.tx.is_closed());
        // BR-R52: C gateway fires postEvent(VALUE|ALARM|LOG) for every
        // upstream event (gateVc.cc:374-376); widen to match so DBE_LOG
        // archivers and DBE_ALARM-only monitors receive gateway snapshot posts.
        let post = EventMask::VALUE | EventMask::LOG | EventMask::ALARM;
        for sub in subs.iter() {
            if !sub.accepts(post) {
                continue;
            }
            let event = MonitorEvent {
                snapshot: snapshot.clone(),
                origin: 0,
            };
            let filtered = if sub.filters.is_empty() {
                Some(event)
            } else {
                sub.filters
                    .apply(FilteredMonitorEvent::new(event, post))
                    .map(|fe| fe.event)
            };
            let Some(event) = filtered else {
                continue;
            };
            if sub.tx.try_send(event.clone()).is_err() {
                sub.coalesce_overflow(event);
            }
        }
    }

    /// Add a subscriber. Returns the receiver for monitor events,
    /// or `None` when the per-PV subscriber cap has been reached
    /// (P-G14: defends against a misbehaving client opening many
    /// MONITOR ops against one shared PV; per-channel cap limits
    /// channels but not subscriber rows on a single PV). Operators
    /// override the cap via `EPICS_CAS_MAX_SUBSCRIBERS_PER_PV`
    /// (default 1024 — large enough for any realistic dashboard
    /// fan-out, small enough to bound memory under abuse).
    ///
    /// Channel depth defaults to 64 events; the operator can lift the
    /// cap via `EPICS_CAS_MAX_EVENTS_PER_CHAN` for sites that need
    /// deeper coalescing buffers. C rsrv does not advertise this knob
    /// (its queue is internally fixed) — exposing it lets us tune
    /// memory vs latency for slow-viewer workloads.
    pub async fn add_subscriber(
        &self,
        sid: u32,
        data_type: DbFieldType,
        mask: u16,
    ) -> Option<mpsc::Receiver<MonitorEvent>> {
        let cap = max_subscribers_per_pv();
        let (tx, rx) = mpsc::channel(per_channel_event_depth());
        let sub = Subscriber {
            sid,
            data_type,
            mask,
            tx,
            coalesced: Arc::new(StdMutex::new(None)),
            filters: crate::server::database::filters::FilterChain::new(),
        };
        let mut subs = self.subscribers.lock().await;
        // Round 46 (R46-G1): reap dead Senders BEFORE counting
        // against the cap. `notify_subscribers` / `post_alarm`
        // already retain-filter on every emission, but a PV with
        // no value changes (e.g. a static catalog entry that
        // dashboards latch onto and drop) never triggered the
        // reaper — a long-lived subscribe / disconnect storm could
        // pin the Vec at `cap` worth of closed `Sender`s and lock
        // out genuine new subscribers with a false-positive cap-
        // reached warning. Same defect class as the round-37
        // NDPluginPva subscribe reaper (qsrv/pva_adapter.rs:247).
        subs.retain(|s| !s.tx.is_closed());
        if subs.len() >= cap {
            tracing::warn!(
                pv = %self.name,
                live = subs.len(),
                cap,
                "PV subscriber cap reached, refusing add_subscriber"
            );
            return None;
        }
        subs.push(sub);
        Some(rx)
    }

    /// CA-FR-8: attach a channel-filter chain to an already-added
    /// subscriber (looked up by `sid`). The CA server first
    /// `add_subscriber`s, then attaches the chain parsed from the
    /// channel's `.{...}` suffix — symmetric with the record-field
    /// `RecordInstance::attach_filter_to_last_subscriber` path, so a
    /// `SimplePv` monitor runs the SAME filter chain as a record-field
    /// monitor instead of the empty default `FilterChain` that
    /// `add_subscriber` installs. Update delivery
    /// ([`Self::notify_subscribers`] / [`Self::post_alarm`]) already
    /// applies `sub.filters`; this is the missing wiring that populates
    /// it.
    ///
    /// The caller passes a FRESH chain per subscriber so stateful
    /// filters (`dbnd` last-value, `dec` counter, `sync` state) stay
    /// isolated across subscribers. An empty chain is a no-op (keeps the
    /// default). No-op when no subscriber matches `sid` (e.g. it was
    /// reaped between add and attach).
    pub async fn attach_filters_to_subscriber(
        &self,
        sid: u32,
        filters: crate::server::database::filters::FilterChain,
    ) {
        if filters.is_empty() {
            return;
        }
        let mut subs = self.subscribers.lock().await;
        if let Some(sub) = subs.iter_mut().find(|s| s.sid == sid) {
            sub.filters = filters;
        }
    }

    /// Remove a subscriber by subscription ID.
    pub async fn remove_subscriber(&self, sid: u32) {
        let mut subs = self.subscribers.lock().await;
        subs.retain(|s| s.sid != sid);
    }

    /// Take any pending coalesced overflow value for the given subscriber.
    /// Called by the per-subscription forwarder task after each delivery
    /// so a slow consumer always converges on the latest known value.
    pub async fn pop_coalesced(&self, sid: u32) -> Option<MonitorEvent> {
        let subs = self.subscribers.lock().await;
        let sub = subs.iter().find(|s| s.sid == sid)?;
        sub.coalesced.lock().ok()?.take()
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

    /// CA-FR-6: a `DBE_ALARM`-only subscriber must not receive a plain
    /// value set, but must receive an alarm post.
    #[tokio::test]
    async fn alarm_only_subscriber_skips_value_post() {
        let pv = pv();
        let mut rx = pv
            .add_subscriber(1, DbFieldType::Double, DBE_ALARM)
            .await
            .expect("subscriber added");
        pv.set(EpicsValue::Double(1.0)).await;
        assert!(
            rx.try_recv().is_err(),
            "DBE_ALARM-only subscriber must not receive a value post"
        );
        pv.post_alarm(2, 3).await;
        assert!(
            rx.try_recv().is_ok(),
            "DBE_ALARM subscriber must receive an alarm post"
        );
    }

    /// CA-FR-6: a `DBE_VALUE`-only subscriber must not receive a
    /// `post_alarm`, but must receive value sets.
    #[tokio::test]
    async fn value_only_subscriber_skips_alarm_post() {
        let pv = pv();
        let mut rx = pv
            .add_subscriber(1, DbFieldType::Double, DBE_VALUE)
            .await
            .expect("subscriber added");
        pv.post_alarm(2, 3).await;
        assert!(
            rx.try_recv().is_err(),
            "DBE_VALUE-only subscriber must not receive an alarm post"
        );
        pv.set(EpicsValue::Double(1.0)).await;
        assert!(
            rx.try_recv().is_ok(),
            "DBE_VALUE subscriber must receive a value post"
        );
    }

    // --- BR-R52 regression: set_snapshot must reach DBE_LOG and DBE_ALARM-only subs ---

    fn snapshot() -> Snapshot {
        Snapshot::new(
            EpicsValue::Double(2.0),
            0,
            0,
            std::time::SystemTime::UNIX_EPOCH,
        )
    }

    /// BR-R52: a DBE_LOG (archiver) subscriber must receive a set_snapshot post.
    #[tokio::test]
    async fn log_subscriber_receives_snapshot_post() {
        let pv = pv();
        let mut rx = pv
            .add_subscriber(1, DbFieldType::Double, DBE_LOG)
            .await
            .expect("subscriber added");
        pv.set_snapshot(snapshot()).await;
        assert!(
            rx.try_recv().is_ok(),
            "DBE_LOG subscriber must receive a set_snapshot post"
        );
    }

    /// BR-R52: a DBE_ALARM-only subscriber must receive a set_snapshot post.
    #[tokio::test]
    async fn alarm_only_subscriber_receives_snapshot_post() {
        let pv = pv();
        let mut rx = pv
            .add_subscriber(1, DbFieldType::Double, DBE_ALARM)
            .await
            .expect("subscriber added");
        pv.set_snapshot(snapshot()).await;
        assert!(
            rx.try_recv().is_ok(),
            "DBE_ALARM-only subscriber must receive a set_snapshot post"
        );
    }

    /// BR-R52: a DBE_VALUE subscriber must still receive a set_snapshot post.
    #[tokio::test]
    async fn value_subscriber_receives_snapshot_post() {
        let pv = pv();
        let mut rx = pv
            .add_subscriber(1, DbFieldType::Double, DBE_VALUE)
            .await
            .expect("subscriber added");
        pv.set_snapshot(snapshot()).await;
        assert!(
            rx.try_recv().is_ok(),
            "DBE_VALUE subscriber must receive a set_snapshot post"
        );
    }

    /// A `DBE_VALUE | DBE_ALARM` subscriber receives both event classes.
    #[tokio::test]
    async fn both_classes_receive_both_posts() {
        let pv = pv();
        let mut rx = pv
            .add_subscriber(1, DbFieldType::Double, DBE_VALUE | DBE_ALARM)
            .await
            .expect("subscriber added");
        pv.set(EpicsValue::Double(1.0)).await;
        assert!(rx.try_recv().is_ok(), "value post delivered to VALUE|ALARM");
        pv.post_alarm(2, 3).await;
        assert!(rx.try_recv().is_ok(), "alarm post delivered to VALUE|ALARM");
    }

    /// BR-R52: a DBE_LOG-only subscriber (archiver) must receive both value
    /// events and alarm events.  Pre-fix: VALUE-only / ALARM-only post masks
    /// never intersected DBE_LOG(2), so archivers received silence.
    #[tokio::test]
    async fn br_r52_log_subscriber_receives_value_and_alarm_events() {
        const DBE_LOG: u16 = 2;
        let pv = pv();
        let mut rx = pv
            .add_subscriber(1, DbFieldType::Double, DBE_LOG)
            .await
            .expect("subscriber added");
        pv.set(EpicsValue::Double(1.0)).await;
        assert!(
            rx.try_recv().is_ok(),
            "DBE_LOG subscriber must receive a value post"
        );
        pv.post_alarm(2, 3).await;
        assert!(
            rx.try_recv().is_ok(),
            "DBE_LOG subscriber must receive an alarm post"
        );
    }
}
