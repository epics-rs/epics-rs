//! Per-PV upstream-channel cache for the PVA gateway.
//!
//! Mirrors `pva2pva/p2pApp/chancache.{h,cpp}` `ChannelCache` /
//! `ChannelCacheEntry` — a deduplicated map keyed by PV name. Each
//! entry owns one upstream connection and one upstream monitor task
//! (spun up on first interest, kept alive for the entry's lifetime),
//! plus a tokio broadcast channel that fans the upstream values out
//! to every downstream subscriber.
//!
//! The C++ version uses `epicsTimer` to expire entries that have lost
//! all interest; we use a simple periodic sweep (default 30 s) over
//! the map and prune entries whose `drop_poke` is false AND whose
//! broadcast sender has zero receivers. Downstream `subscribe()` calls
//! re-set `drop_poke = true` so a repeatedly-asked PV stays alive even
//! between bursts.
//!
//! "Interest" here means downstream MONITOR interest specifically — a
//! narrower predicate than C's `interested` (the set of ALL open
//! downstream channels, `chancache.cpp:121`). This is deliberate: the
//! gateway's one-shot GET / PUT / RPC / introspection ops bypass this
//! cache entirely (they would otherwise spawn a shared-identity upstream
//! monitor as a side effect), so only the MONITOR path ever creates an
//! entry. See `UpstreamEntry::is_retained` for the full rationale and
//! the (Low) cost of the narrowing.

// RTEMS-EXEC-MODEL-ALLOW(7): checked to pass on the exec backend under
// `EPICS_RS_BUILD_EXEC_BACKEND=thread cargo nextest run -p epics-bridge-rs
// --features pva-gateway` (the gateway's spawns/timers ride the runtime::task
// seam; 739 run, 739 passed on this tree). The default exec-backend gate omits
// `pva-gateway`, so re-run that combo when touching this module.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use parking_lot::RwLock;
use tokio::sync::{Mutex, Notify, broadcast};

use epics_base_rs::runtime::task::Reactor;
use epics_pva_rs::client::PvaClient;
use epics_pva_rs::pvdata::{FieldDesc, PvField};
use epics_pva_rs::server_native::MonitorUpdate;
use epics_pva_rs::server_native::source::{ChannelInvalidator, SourceRead};

use super::error::{GwError, GwResult};

/// Default broadcast channel capacity. Matches the pvxs default
/// downstream queueSize of 16. A slow downstream subscriber that
/// can't keep up will see lagged events; the next successful
/// upstream tick brings it back into sync.
pub const BROADCAST_CAPACITY: usize = 16;

/// Default cache cleanup period — matches p2pApp `cacheClean` 30 s.
pub const DEFAULT_CLEANUP_INTERVAL: Duration = Duration::from_secs(30);

/// Default ceiling on cached entries. A misbehaving client searching
/// random PV names would otherwise cause the cache to grow until
/// `cleanup_interval` fires, holding one upstream-monitor task per
/// entry. 50 000 is comfortably above any real IOC's PV count and
/// well below typical heap/socket budgets.
pub const DEFAULT_MAX_ENTRIES: usize = 50_000;

// No negative-result cache: pva2pva's `ChannelCache::lookup`
// (`p2pApp/chancache.cpp:166-206`) keeps a failed/not-yet-connected
// name as a LIVE cache entry whose upstream channel observes a later
// connection — it has no negative-admission table that suppresses
// re-probing for a fixed TTL. A 30 s negative LRU here made a PV that
// appears shortly after a failed search stay "not found" until the TTL
// lapsed or an operator dropped the name. The
// probe-storm DoS the LRU originally guarded was structurally closed
// separately: existence probes (`has_pv` / `get_introspection`) no
// longer spawn upstream monitor tasks (they issue one-shot
// `pvconnect` / `pvinfo`), and a failed `lookup` removes its fresh
// entry immediately via the `CleanupGuard` rather than pinning a task.

/// Per-PV upstream entry. One entry → one upstream channel → one
/// upstream monitor task → N downstream subscribers via broadcast.
pub struct UpstreamEntry {
    pub pv_name: String,
    /// Latest cached value + introspection. Populated on first
    /// upstream monitor event. `Arc<RwLock<…>>` so the monitor task
    /// can write into it without holding a reference to the
    /// `UpstreamEntry` (avoids the chicken-and-egg between the entry
    /// and its background task).
    state: Arc<RwLock<EntryState>>,
    /// Fan-out for upstream monitor events. Subscribers receive a
    /// fresh `broadcast::Receiver` from `subscribe()`. Holding the
    /// sender keeps the channel alive across re-subscribes.
    tx: broadcast::Sender<MonitorUpdate>,
    /// Raw-frame fan-out. Carries the upstream MONITOR DATA
    /// body (`changed | value | overrun`) as a refcounted
    /// `bytes::Bytes` so N downstream subscribers all share the
    /// same allocation. Server-side `subscribe_raw` returns a
    /// receiver from this sender; the monitor task pumps both
    /// `tx` (decoded PvField, for `subscribe()` and snapshot) and
    /// `tx_raw` (raw bytes) per upstream event.
    tx_raw: broadcast::Sender<crate::pva_gateway::source::RawEvent>,
    /// cached latest raw event. Populated after the first
    /// upstream monitor event so `subscribe_raw_inner` can deliver it
    /// as the initial snapshot to new raw subscribers. Cleared on
    /// type-change so stale bytes from the old descriptor are not
    /// replayed under the new one. Mirrors `moncache.cpp` `lastelem`.
    latest_raw: Arc<RwLock<Option<crate::pva_gateway::source::RawEvent>>>,
    /// Pulsed on the first successful upstream event. `lookup()` waits
    /// on this so callers see a populated snapshot before returning.
    first_event: Arc<Notify>,
    /// Background upstream monitor task. Aborted on entry drop.
    _monitor_task: AbortOnDrop,
    /// Sticky "recently used" bit, lowered by the cleanup tick.
    drop_poke: parking_lot::Mutex<bool>,
}

// **Invariant: a downstream monitor never throttles the upstream** (R18-25).
//
// One upstream monitor fans out to every downstream subscriber of the same
// (PV, credential). Nothing a single downstream does may slow that shared
// upstream, because the upstream is not its to slow: its co-subscribers are
// reading the same stream and a pause is not divisible.
//
// This is pva2pva's design, and it is explicit about it —
// `MonitorCacheEntry::monitorEvent` (`moncache.cpp:133-174`) polls the
// upstream dry (`while((update=monitor->poll()))`) with a bare
// `//TODO: flow control` where an upstream throttle would go, and absorbs
// a downstream that cannot keep up *in that downstream's own buffer*: its
// `overflowElement` accumulates the changed/overrun bitsets and takes the
// latest value, `ndropped` counts what was coalesced away, and the upstream
// keeps flowing for everyone else.
//
// The port does the same, structurally: each downstream forwarder owns a
// `broadcast::Receiver` and an mpsc queue, and a slow one lags its OWN
// receiver — `RecvError::Lagged` → `pending_overrun` → the next body it
// forwards carries the overrun mark (`subscribe_raw_inner` /
// `subscribe_inner` in `source.rs`). That is the overflow element.
//
// What used to be here — a `PauseControl` that reference-counted per-op
// pipeline-window votes and drove the upstream client's `Pauser` — could not
// be made correct: an op became a *voter* only by crossing its own LOW
// watermark, so a non-pipelined downstream never voted, and the aggregate
// "pause iff every voting op wants pause" then ignored it entirely. One slow
// pipelined client paused the shared upstream and every healthy co-subscriber
// starved with no way to resume it. Deleting the throttle is what closes that
// family: there is no sink to drive, so no vote can strand anyone.

#[derive(Default)]
struct EntryState {
    /// Most recent value seen on the upstream monitor: the merge of every
    /// upstream event since the snapshot began (a first event, an
    /// introspection change, or a disconnect starts a new one). A delta
    /// event is merged onto the prior snapshot by
    /// `fill_unmarked_from_prior`, so this is always a whole structure.
    ///
    /// It carries no mark set of its own, because a monitor seed built from
    /// it does not declare one: pva2pva's seed is `elem->changedBitSet->set(0)`
    /// — the ROOT bit, "all changed" (`moncache.cpp:304-312`). See
    /// `monitor_seed`, the one owner of that rule.
    latest: Option<PvField>,
    /// Type descriptor learned from the first INIT response.
    introspection: Option<FieldDesc>,
}

/// The monitor seed for a cached snapshot — the ONE place the gateway
/// decides what a downstream seed declares (R18-28).
///
/// pva2pva seeds a starting `MonitorUser` with the cache's merged element
/// and marks the **root** bit: `elem->pvStructurePtr->copy(*lval);
/// elem->changedBitSet->set(0); // indicate all changed`
/// (`moncache.cpp:304-312`) — *not* the union of the upstream marks that
/// built `lval`. Our encoder cannot emit bit 0 (`pvdata/encode.rs`
/// trims to leaves), so the decodable equivalent is the canonical full leaf
/// bitset, which is exactly what `SourceRead { marked: None }` frames.
/// Routing every seed through this function makes "a seed is whole-structure"
/// hold by construction: no caller can hand the server a partial mask.
///
/// A `None` return means no upstream event has arrived yet (pva2pva's
/// `!havedata`, `moncache.cpp:285` + `:304`), so there is no seed frame at
/// all: the downstream monitor's first frame is the first upstream update.
fn monitor_seed(state: &RwLock<EntryState>) -> Option<SourceRead> {
    state.read().latest.clone().map(SourceRead::from)
}

/// Result of folding one upstream monitor event into [`EntryState`].
#[derive(Debug, Default, Clone)]
struct MonitorEventOutcome {
    /// This was the first event — `first_event` waiters should wake.
    was_first: bool,
    /// The upstream introspection changed vs. the cached descriptor.
    type_changed: bool,
    /// The merged value after decode+merge, read under the state write lock.
    /// `None` when the body could not be decoded.
    value: Option<PvField>,
    /// The upstream event's changed-leaf field paths, decoded from the
    /// frame's `changed` bitset. `None` means "let the server derive the
    /// changed-bitset" — used for a whole-structure (root-bit) event and
    /// when the body could not be decoded. `Some(paths)` carries the real
    /// changed leaves so the downstream cooked monitor advertises the
    /// upstream's actual changed-bitset, not a synthesised full mask.
    ///
    /// This is the UPDATE path, and only the update path: pva2pva copies the
    /// upstream event's changed bitset onto each downstream element
    /// (`moncache.cpp:142` `*lastelem->changedBitSet = *update->changedBitSet`,
    /// `:187` the per-user copy). The SEED is a different rule entirely — the
    /// root bit, see `monitor_seed`.
    marked: Option<Vec<String>>,
}

/// Decode one upstream raw monitor frame and fold it into `state`.
///
/// BUG 2: this runs for EVERY upstream monitor event so a gateway GET
/// (`UpstreamEntry::snapshot` → `get_value`) always returns the
/// CURRENT upstream value. Pre-fix the callback decoded only the first
/// event, freezing `state.latest` forever.
///
/// `body` is the wire `changed | value | overrun` triplet. A delta
/// event carries only the changed fields; `decode_pv_field_with_bitset`
/// zero-fills the unmarked leaves, so when a prior snapshot exists (and
/// the introspection is unchanged) the decoded delta is merged onto it
/// via `fill_unmarked_from_prior` — the same merge the client-side
/// `pvmonitor` applies. A first event or an introspection change
/// replaces `state.latest` wholesale.
fn apply_monitor_event(
    state: &RwLock<EntryState>,
    desc: &FieldDesc,
    body: &[u8],
    order: epics_pva_rs::proto::ByteOrder,
) -> MonitorEventOutcome {
    let decoded = (|| -> Option<(epics_pva_rs::proto::BitSet, PvField)> {
        let mut cur = std::io::Cursor::new(body);
        let changed = epics_pva_rs::proto::BitSet::decode(&mut cur, order).ok()?;
        let v = epics_pva_rs::pvdata::encode::decode_pv_field_with_bitset(
            desc, &changed, 0, &mut cur, order,
        )
        .ok()?;
        Some((changed, v))
    })();

    let Some((changed, v)) = decoded else {
        // Decode failure (truncated / malformed / descriptor-inconsistent
        // frame). The event still ARRIVED: C marks `havedata = true`
        // unconditionally at the top of MonitorCacheEntry::monitorEvent
        // (moncache.cpp:132-133), before its copy loop, so the
        // first-event/`havedata` signal does not depend on a successful
        // decode. Mirror that — report `was_first` when no prior event has
        // been seen so `raw_cb` fires `first_event` and `await_first_event`
        // does not time out and evict a connectable PV as not-found. There
        // is no decoded value to merge, so `value` stays `None` and
        // `state.latest` is left untouched; the raw frame itself is still
        // cached and fanned out by the caller regardless of this decode.
        return MonitorEventOutcome {
            was_first: state.read().introspection.is_none(),
            type_changed: false,
            value: None,
            marked: None,
        };
    };

    // Translate the upstream event's `changed` bitset into the changed-leaf
    // field paths so the downstream cooked monitor advertises the real
    // changed-bitset (pva2pva copies `*update->changedBitSet` verbatim,
    // moncache.cpp:142,187) rather than a synthesised full mask. A set root
    // bit (0) means the whole structure changed — represented as `None`
    // (full mask) since the root has no path.
    let marked = if changed.get(0) {
        None
    } else {
        Some(epics_pva_rs::pvdata::encode::changed_bitset_paths(
            desc, &changed,
        ))
    };

    let mut s = state.write();
    let was_first = s.introspection.is_none();
    let type_changed = s
        .introspection
        .as_ref()
        .is_some_and(|existing| existing != desc);
    s.introspection = Some(desc.clone());
    match s.latest.take() {
        Some(prior) if !type_changed => {
            s.latest = Some(epics_pva_rs::pvdata::encode::fill_unmarked_from_prior(
                desc, &changed, 0, v, &prior,
            ));
        }
        _ => s.latest = Some(v),
    }
    // Read merged value while still holding the write lock so callers
    // receive the exact value that was stored — no separate re-acquisition.
    let value = s.latest.clone();
    MonitorEventOutcome {
        was_first,
        type_changed,
        value,
        marked,
    }
}

/// Emit a subscription-boundary marker to **both** downstream fanout
/// streams — the raw `tx_raw` (`RawEvent { type_changed: true }`) and the
/// decoded `tx` (`MonitorUpdate::type_change()`). Single owner of the
/// "a boundary reaches every downstream monitor" invariant: a downstream
/// monitor takes the raw fast path only with a full field mask, no
/// pipeline, and no server-side filter — otherwise it is on the decoded
/// path. A boundary (upstream descriptor change or disconnect) MUST reach
/// it on whichever stream it subscribed, or it keeps serving values under
/// the stale INIT descriptor. Both emit sites — the
/// descriptor-change branch in `spawn_upstream_monitor`'s callback and
/// `signal_disconnect_boundary` — route through here so neither can notify
/// one stream and silently drop the other.
fn broadcast_boundary(
    tx: &broadcast::Sender<MonitorUpdate>,
    tx_raw: &broadcast::Sender<crate::pva_gateway::source::RawEvent>,
    byte_order: epics_pva_rs::proto::ByteOrder,
) {
    let _ = tx_raw.send(crate::pva_gateway::source::RawEvent {
        body: bytes::Bytes::new(),
        byte_order,
        type_changed: true,
    });
    let _ = tx.send(MonitorUpdate::type_change());
}

/// Surface an upstream disconnect to downstream monitors as a
/// subscription boundary, mirroring pva2pva `moncache.cpp:212-235`
/// (`MonitorCacheEntry`'s lost upstream → downstream *unlisten* / MONITOR
/// FINISH, **not** a fabricated alarm value). Reuses the same empty
/// `type_changed` marker the descriptor-change path emits (the
/// `outcome.type_changed` branch in `spawn_upstream_monitor`) via
/// [`broadcast_boundary`], so the boundary reaches BOTH raw and decoded
/// downstream subscribers: the native server turns it into MONITOR FINISH
/// (`server_native/tcp.rs` `build_monitor_finish`) so each downstream
/// re-opens with a fresh INIT.
///
/// Also clears the cached snapshot (`latest_raw` + `state.latest`): after
/// the boundary there is no live value, so a subscriber attaching during
/// the outage waits for the reconnect's first frame instead of being
/// served stale data flagged live.
///
/// Idempotent within one outage *by construction*: it no-ops when no
/// snapshot is cached, and the first call clears it — so repeated
/// re-subscribe failures during a single outage emit FINISH at most once,
/// while the next reconnect event repopulates `latest_raw`, re-arming the
/// boundary for the following disconnect. This invariant replaces the
/// former `disconnected_alarm_sent` flag.
fn signal_disconnect_boundary(
    state: &RwLock<EntryState>,
    latest_raw: &RwLock<Option<crate::pva_gateway::source::RawEvent>>,
    tx: &broadcast::Sender<MonitorUpdate>,
    tx_raw: &broadcast::Sender<crate::pva_gateway::source::RawEvent>,
) {
    // No cached snapshot ⇒ nothing was delivered downstream this connection
    // cycle (or the boundary already fired and cleared it): nothing to revoke.
    let byte_order = match latest_raw.read().as_ref() {
        Some(ev) => ev.byte_order,
        None => return,
    };
    *latest_raw.write() = None;
    state.write().latest = None;
    broadcast_boundary(tx, tx_raw, byte_order);
}

impl UpstreamEntry {
    /// The downstream monitor seed for this entry's cached snapshot — the
    /// merged upstream value, declared whole-structure, or `None` when no
    /// upstream event has arrived yet. See `monitor_seed` for the rule and
    /// its pva2pva citation; this is a thin accessor so that rule has exactly
    /// one owner.
    pub fn snapshot(&self) -> Option<SourceRead> {
        monitor_seed(&self.state)
    }

    /// Cached introspection if known.
    pub fn introspection(&self) -> Option<FieldDesc> {
        self.state.read().introspection.clone()
    }

    /// Subscribe to upstream events. The receiver is fresh — pre-existing
    /// values are NOT replayed (broadcast semantics). Callers needing
    /// the current value should also call [`Self::snapshot`]. The stream
    /// carries `MonitorUpdate` so an upstream descriptor change reaches
    /// decoded subscribers as a `type_changed` boundary, the
    /// decoded-path counterpart of [`Self::subscribe_raw`]'s `RawEvent`.
    pub fn subscribe(&self) -> broadcast::Receiver<MonitorUpdate> {
        self.poke();
        self.tx.subscribe()
    }

    /// Raw-frame subscriber. Receives upstream MONITOR DATA
    /// body bytes verbatim. Server uses this to skip its own
    /// `encode_pv_field` step.
    pub fn subscribe_raw(&self) -> broadcast::Receiver<crate::pva_gateway::source::RawEvent> {
        self.poke();
        self.tx_raw.subscribe()
    }

    /// latest cached raw upstream frame. Returns `None` until
    /// the first upstream monitor event has been received (or after a
    /// type-change resets the cache).
    pub fn snapshot_raw(&self) -> Option<crate::pva_gateway::source::RawEvent> {
        self.latest_raw.read().clone()
    }

    /// Number of live downstream subscribers (broadcast receivers).
    pub fn subscriber_count(&self) -> usize {
        // Count both fan-out streams: typed PvField subscribers AND
        // raw-frame subscribers. The upstream monitor task should
        // stay alive when either path has consumers.
        self.tx.receiver_count() + self.tx_raw.receiver_count()
    }

    /// Single keep-predicate for both eviction paths: an entry is retained
    /// when it still holds its one-tick `drop_poke` grace (freshly spawned /
    /// recently looked up) or has at least one live downstream MONITOR
    /// subscriber.
    ///
    /// Deliberate deviation from C, NOT a mirror of it. pva2pva
    /// `cacheClean::expire` evicts on `!dropPoke && interested.empty()`
    /// (`chancache.cpp:121`), where `interested` is the set of ALL open
    /// downstream `GWChannel`s — every channel inserted at `createChannel`
    /// (`p2pApp/server.cpp:75-80`) regardless of whether it does
    /// GET/PUT/MONITOR/RPC. The Rust gateway cannot key retention on that
    /// set: its one-shot
    /// GET / PUT / RPC / introspection ops deliberately bypass this cache
    /// (`source.rs` `get_value` / `put_value*` / `get_introspection*` go
    /// straight to `cache.client()`), because routing them through a cache
    /// entry would spawn a shared-identity upstream monitor as a side effect
    /// — the leak removed in `source.rs` `put_value_checked`. Only the
    /// MONITOR path creates an entry, so retention here is exactly "monitor
    /// interest", by construction. The sole consequence of the narrowing is
    /// that a MONITOR opened after a monitor-less idle gap re-searches and
    /// reconnects the upstream (Low cost); no in-flight op can be stranded,
    /// because non-monitor ops never consult this cache.
    ///
    /// Pure read — it does NOT consume the grace; only `cleanup_tick` resets
    /// `drop_poke`. The cache-full emergency sweep in [`ChannelCache::lookup`] used
    /// `Arc::strong_count > 1`, which does not mean "has a subscriber": a
    /// subscribed-but-not-mid-lookup entry had `strong_count == 1` and was
    /// wrongly swept — silently killing the shared upstream monitor for
    /// every downstream subscriber. Retention is the RECEIVER count, which is
    /// what "monitor interest" actually is; routing both paths through this
    /// predicate makes that divergence unrepresentable, whatever else happens
    /// to hold an `Arc` (a forwarder task keeps one so it can re-frame from
    /// `latest` after a fanout lag).
    fn is_retained(&self) -> bool {
        *self.drop_poke.lock() || self.subscriber_count() > 0
    }

    fn poke(&self) {
        *self.drop_poke.lock() = true;
    }
}

/// Drop guard that aborts a spawned task when the entry is dropped.
struct AbortOnDrop(epics_base_rs::runtime::task::TaskAbortHandle);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// One cached upstream channel — pva2pva `ChannelCacheEntry`
/// (`p2pApp/chancache.h:103-125`). Groups the per-pvRequest upstream
/// monitors opened for a single PV name. The top-level cache
/// ([`ChannelCache::entries`]) is keyed by channel name; `cacheSize`, the
/// admission cap, and the cleaner counters all operate on this level
/// (channels), exactly as pva2pva reports `entries.size()` channels and
/// `cleanerDust` channels (`p2pApp/server.cpp:182-198`). Each channel
/// holds its own nested map of monitor variants keyed by serialized
/// downstream
/// pvRequest (pva2pva `mon_entries`, `chancache.h:123-125`): two
/// downstream monitors that ask for different field sets / `record._options`
/// / `_filter` chains each get their own upstream monitor opened with their
/// own request rather than all sharing one gateway-default stream
/// (`moncache.cpp:34-43`, `p2pApp/channel.cpp:157-224`). An empty
/// pvRequest key is the no-request default fanout (the ctx-less
/// `subscribe`/`subscribe_raw` paths, which carry no downstream request).
///
/// Splitting the two levels keeps `cacheSize` a *channel* count: one PV
/// asked for under three pvRequests is ONE cached channel with three
/// unique subscriptions, not three cache entries — so it consumes one slot
/// of the admission cap and increments `cleanerRemoved` by one when it ages
/// out, matching pva2pva.
struct ChannelEntry {
    /// Upstream monitor variants for this channel, keyed by serialized
    /// downstream pvRequest bytes (see [`pv_request_key`]). pva2pva
    /// `ChannelCacheEntry::mon_entries`.
    monitors: HashMap<Vec<u8>, Arc<UpstreamEntry>>,
    /// Connection-only admission recency for a `has_pv`/`pvconnect` channel
    /// that connected upstream without opening a monitor. The bool is the
    /// recency grace, lowered one cleaner tick at a time exactly like
    /// [`UpstreamEntry::drop_poke`] (`Some(true)` → consume grace,
    /// `Some(false)` → evict), so a connectionless-probe channel is counted
    /// in `cacheSize`, charged against the admission cap, and reachable by
    /// `:drop`/`:flush` until it ages out. pva2pva caches the connected
    /// channel from `channelFind` so it lives in `entries` and the cleaner
    /// (`chancache.cpp:166-199`); the actual upstream channel is released by
    /// the client's own idle reaper. `None` for a pure monitor channel.
    connection: Option<bool>,
}

impl ChannelEntry {
    fn new() -> Self {
        Self {
            monitors: HashMap::new(),
            connection: None,
        }
    }

    /// Live downstream subscribers summed across this channel's monitor
    /// variants (decoded + raw fan-outs of each variant).
    fn subscriber_count(&self) -> usize {
        self.monitors.values().map(|e| e.subscriber_count()).sum()
    }

    /// `true` once any variant's upstream has delivered ≥1 event (its
    /// snapshot is populated) — pva2pva per-channel `haveData` — or the
    /// channel was admitted by a successful connection-only probe.
    fn connected(&self) -> bool {
        self.connection.is_some()
            || self
                .monitors
                .values()
                .any(|e| e.state.read().latest.is_some())
    }

    /// `true` while any variant still holds its `drop_poke` recency grace.
    fn drop_poke(&self) -> bool {
        self.monitors.values().any(|e| *e.drop_poke.lock())
    }
}

/// Remove the monitor variant `req_key` from `pv_name`'s channel, dropping
/// the channel entry itself when it has no remaining variants. The single
/// shape used by every variant-removal site (the lookup cleanup guard) so
/// an emptied channel never lingers as a zero-variant ghost in `cacheSize`.
fn remove_variant(map: &mut HashMap<String, ChannelEntry>, pv_name: &str, req_key: &[u8]) {
    if let Some(channel) = map.get_mut(pv_name) {
        channel.monitors.remove(req_key);
        if channel.monitors.is_empty() {
            map.remove(pv_name);
        }
    }
}

/// Serialize a downstream pvRequest VALUE into the canonical key bytes for
/// a [`ChannelEntry`]'s nested monitor-variant map. Fixed little-endian so
/// the key is stable regardless of the downstream connection's byte order —
/// it is a dedup key, never sent on the wire (the forward path re-encodes
/// per upstream connection). `None` (no pvRequest captured) maps to the
/// empty default-fanout key.
fn pv_request_key(pv_request: Option<&PvField>) -> Vec<u8> {
    match pv_request {
        Some(req) => epics_pva_rs::client_native::ops_v2::encode_pv_request_value(
            req,
            epics_pva_rs::proto::ByteOrder::Little,
        ),
        None => Vec::new(),
    }
}

/// Decide the re-subscribe delay for [`ChannelCache::spawn_upstream_monitor`]'s
/// reconnect loop after an upstream monitor subscription ends.
///
/// The returned `delay` is slept UNCONDITIONALLY before the loop re-issues
/// the upstream monitor — on a clean `MONITOR FINISH` (the wait returned
/// `Ok`) exactly as on a disconnect/error (`Err`). Without it a clean
/// FINISH (the upstream channel stays Active, only the IOID is
/// unregistered) would tight-loop INIT→FINISH→resubscribe with zero delay,
/// spinning the CPU. pva2pva treats unlisten as terminal
/// (moncache.cpp:214-236); the gateway reconnects transparently but must
/// never do so with zero delay.
///
/// `event_seen` is whether the just-ended subscription delivered at least
/// one upstream event during its lifetime:
/// - `true` (a healthy connection that dropped): the delay re-arms the
///   `floor` so the reconnect is prompt — mirrors ca_gateway/upstream.rs:896.
/// - `false` (a clean FINISH that ended without ever delivering an event,
///   or an immediate end): the delay keeps the grown `backoff` so repeated
///   immediate ends back off geometrically instead of spinning.
///
/// Returns `(delay, next_backoff)`: `delay` is slept now; `next_backoff` is
/// the running backoff for the following iteration, doubled toward `max`.
/// The delay is never zero — `floor` and `backoff` are both positive — so a
/// clean FINISH cannot busy-loop.
fn resubscribe_backoff(
    event_seen: bool,
    backoff: Duration,
    floor: Duration,
    max: Duration,
) -> (Duration, Duration) {
    let delay = if event_seen { floor } else { backoff };
    let next_backoff = std::cmp::min(delay * 2, max);
    (delay, next_backoff)
}

/// Process-wide cache. Handed to the gateway server source as an
/// `Arc<ChannelCache>`; cheap to clone (only the Arc is bumped).
pub struct ChannelCache {
    /// The executor every task this cache owns is spawned through — the
    /// cleanup tick, each upstream monitor, and the contended-lock orphan
    /// reaper in `lookup_with_request`'s drop guard. Held as a value so a
    /// `Drop` impl and the `ChannelSource` trait methods, neither of which
    /// can take an extra parameter, still spawn through a capability the
    /// caller had to prove it holds.
    reactor: Reactor,
    client: Arc<PvaClient>,
    /// Top-level cache, keyed by channel (PV) name — pva2pva
    /// `ChannelCache::entries` (`p2pApp/chancache.cpp:165-209`). Each
    /// [`ChannelEntry`] nests the per-pvRequest upstream monitors for that
    /// name, so `cacheSize`, the admission cap, and the cleaner counters
    /// all operate on channels, not monitor variants.
    entries: Arc<Mutex<HashMap<String, ChannelEntry>>>,
    /// Cleanup-tick handle. Aborted on `ChannelCache` drop.
    cleanup_task: parking_lot::Mutex<Option<epics_base_rs::runtime::task::TaskHandle<()>>>,
    /// Hard cap on the number of cached *channels* (`entries.len()`) —
    /// defends against probe-storm DoS where a client searches N random
    /// names and forces N upstream channels. A new channel past this limit
    /// returns `GwError::CacheFull`; adding another pvRequest *variant* to
    /// an already-cached channel does NOT consume a slot, matching pva2pva
    /// where the cap is on `ChannelCacheEntry` count, not `mon_entries`
    /// (a per-name variant accumulation is bounded instead by each idle
    /// variant aging out of its channel's nested map via the cleaner).
    max_entries: usize,
    /// Lifetime count of cleanup-tick sweeps (pva2pva `cleanerRuns`,
    /// `p2pApp/chancache.cpp:230-262`). Surfaced in the control status
    /// report so an operator can confirm the idle-eviction loop is live.
    cleaner_runs: AtomicU64,
    /// Lifetime count of entries evicted by the cleanup tick (pva2pva
    /// `cleanerDust`, same path). Distinguishes "cache shrank because
    /// idle entries aged out" from "downstream dropped its interest".
    cleaner_removed: AtomicU64,
    /// Server-wide channel invalidator, wired once by the bound
    /// `GatewayChannelSource` from the native server (see
    /// `ChannelSource::set_channel_invalidator`). An operator
    /// `<prefix>:drop` / `:flush` removes cache entries through [`Self::flush`]
    /// / [`Self::drop_entry`] — the single removal owner — which then
    /// publishes the removed PV names here (one batch per command) so every
    /// connection task force-disconnects the matching downstream channel. This
    /// is the downstream effect of pva2pva dropping a `ChannelCacheEntry`
    /// (`channel->destroy()` → `channelStateChange(DESTROYED)` fanout,
    /// chancache.cpp:34-99). Delivery is lossless by construction, so even a
    /// full-cache `:flush` cannot drop a name. Unset
    /// until wired (standalone caches in tests, or before the server attaches).
    invalidator: OnceLock<ChannelInvalidator>,
}

/// One cached channel's status row for the control report.
#[derive(Debug, Clone)]
pub struct EntryStatus {
    pub pv_name: String,
    /// Upstream has delivered ≥1 event (snapshot populated) on any of this
    /// channel's monitor variants.
    pub connected: bool,
    /// Live downstream subscribers across both fan-outs (decoded + raw),
    /// summed over this channel's monitor variants.
    pub subscribers: usize,
    /// Distinct downstream pvRequest variants (upstream monitors) open for
    /// this channel — pva2pva's per-channel "`<n>` unique subscription(s)"
    /// (`p2pApp/server.cpp:218,228`).
    pub subscriptions: usize,
    /// Idle-eviction grace bit (set while any variant is still poked).
    pub drop_poke: bool,
}

/// One cache's status for the control report: cleaner counters plus a
/// (possibly truncated) list of per-entry rows.
#[derive(Debug, Clone)]
pub struct CacheStatus {
    /// Total cached entries (before any row truncation).
    pub total: usize,
    /// Rows omitted from `entries` due to the row cap.
    pub truncated: usize,
    pub entries: Vec<EntryStatus>,
    pub cleaner_runs: u64,
    pub cleaner_removed: u64,
    pub max_entries: usize,
}

impl ChannelCache {
    /// Build a cache that will route upstream requests through `client`.
    /// Spawns a periodic cleanup task with the given interval; pass
    /// [`DEFAULT_CLEANUP_INTERVAL`] to match p2pApp's 30 s. The
    /// resulting cache uses [`DEFAULT_MAX_ENTRIES`] for its ceiling;
    /// override via [`Self::with_max_entries`] before publishing the
    /// `Arc` if a larger or smaller cap is needed.
    pub fn new(reactor: Reactor, client: Arc<PvaClient>, cleanup_interval: Duration) -> Arc<Self> {
        Self::with_max_entries(reactor, client, cleanup_interval, DEFAULT_MAX_ENTRIES)
    }

    /// Variant of [`Self::new`] with an explicit max-entries cap.
    pub fn with_max_entries(
        reactor: Reactor,
        client: Arc<PvaClient>,
        cleanup_interval: Duration,
        max_entries: usize,
    ) -> Arc<Self> {
        let cache = Arc::new(Self {
            reactor,
            client,
            entries: Arc::new(Mutex::new(HashMap::new())),
            cleanup_task: parking_lot::Mutex::new(None),
            max_entries,
            cleaner_runs: AtomicU64::new(0),
            cleaner_removed: AtomicU64::new(0),
            invalidator: OnceLock::new(),
        });
        let weak = Arc::downgrade(&cache);
        let task = cache.reactor.spawn(async move {
            let mut tick = epics_base_rs::runtime::task::interval(cleanup_interval);
            tick.tick().await; // skip first immediate tick
            loop {
                tick.tick().await;
                let Some(c) = weak.upgrade() else { break };
                c.cleanup_tick().await;
            }
        });
        *cache.cleanup_task.lock() = Some(task);
        cache
    }

    /// Public accessor for the underlying client. Used by the source
    /// to issue one-shot GET / PUT through the same connection pool.
    pub fn client(&self) -> &Arc<PvaClient> {
        &self.client
    }

    /// The executor this cache spawns through. Callers that own an
    /// `Arc<ChannelCache>` — the gateway source, the control surface —
    /// take their capability from here rather than re-deriving one.
    pub fn reactor(&self) -> &Reactor {
        &self.reactor
    }

    /// Cheap, non-spawning probe for "is this PV in the cache right
    /// now?". Returns the cached entry if present (and pokes its
    /// recency bit), or `None` without inserting / spawning.
    ///
    /// Used by paths that act on an ALREADY-cached entry without wanting to
    /// resolve a miss, and simply skip when the entry is gone. The full
    /// `lookup` path remains for
    /// `has_pv`/`get_value`/`subscribe`/`is_writable` etc. that genuinely need
    /// to resolve (connect) the PV.
    pub async fn peek(&self, pv_name: &str) -> Option<Arc<UpstreamEntry>> {
        let map = self.entries.lock().await;
        // Existence probe by PV name: return any cached monitor variant for
        // the channel — callers here need only the entry, not a specific
        // request variant.
        let existing = map.get(pv_name)?.monitors.values().next()?.clone();
        existing.poke();
        Some(existing)
    }

    /// Name-only lookup — the default-fanout entry (empty pvRequest).
    /// Used by existence-warming paths that carry no downstream request
    /// (the ctx-less `subscribe`/`subscribe_raw` and existence probes).
    /// Delegates to [`Self::lookup_with_request`] with `None`.
    pub async fn lookup(
        &self,
        pv_name: &str,
        connect_timeout: Duration,
    ) -> GwResult<Arc<UpstreamEntry>> {
        self.lookup_with_request(pv_name, None, connect_timeout)
            .await
    }

    /// Look up or create the entry for `pv_name` under the downstream
    /// `pv_request` (`None` = the default-fanout key). Each distinct
    /// `(pv_name, serialized pv_request)` shares exactly one upstream
    /// monitor, opened with that request — pva2pva keys its monitor
    /// cache the same way (`p2pApp/moncache.cpp:34-43`,
    /// `p2pApp/channel.cpp:157-224`). Waits up to `connect_timeout` for the
    /// first upstream event so downstream callers see a populated
    /// `snapshot()` before this returns. Mirrors `pva2pva
    /// ChannelCache::lookup` blocking on `isConnected()`.
    ///
    /// Concurrency: spawn-and-insert happens under the same lock, so
    /// two concurrent lookups for the same key cannot each spawn an
    /// upstream monitor task. The wait for the first upstream event
    /// happens AFTER the lock is released so the lock is never held
    /// across the network round-trip.
    ///
    /// **Not-connected handling**: if the upstream never delivers a
    /// first event within `connect_timeout`, the freshly-inserted
    /// entry is removed before returning the error. This prevents a
    /// search storm vector where a typo'd PV name would otherwise
    /// pin an upstream-monitor task on every call until the next 30 s
    /// cleanup tick (review §3f). A subsequent lookup for the same key
    /// re-probes upstream — there is no negative-admission TTL that
    /// would keep a now-available PV "not found" (pva2pva parity, see
    /// the module-level note).
    ///
    /// **Cancel safety**: cleanup of the freshly-inserted entry uses
    /// a drop guard so an awaiting future being cancelled
    /// (`tokio::select!` losing, deadline-exceeded wrapper, etc.) does
    /// not leave the cache pinned.
    pub async fn lookup_with_request(
        &self,
        pv_name: &str,
        pv_request: Option<&PvField>,
        connect_timeout: Duration,
    ) -> GwResult<Arc<UpstreamEntry>> {
        let req_key = pv_request_key(pv_request);
        let (entry, was_fresh) = {
            let mut map = self.entries.lock().await;
            // Existing variant for this (channel, request)? `.cloned()` ends
            // the immutable borrow so the else-branch can mutate `map`.
            let existing = map
                .get(pv_name)
                .and_then(|ch| ch.monitors.get(&req_key))
                .cloned();
            if let Some(existing) = existing {
                existing.poke();
                (existing, false)
            } else {
                // The admission cap counts CHANNELS (pva2pva
                // `ChannelCacheEntry` count), so only a brand-new channel
                // may be refused; adding another pvRequest variant to an
                // already-cached channel never trips it.
                let is_new_channel = !map.contains_key(pv_name);
                if is_new_channel && map.len() >= self.max_entries {
                    // spurious-reject mitigation: pre-sweep the channels the
                    // periodic `cleanup_tick` would also evict — every
                    // variant idle (no remaining `drop_poke` grace AND no
                    // live downstream subscriber). Shares the `is_retained`
                    // keep-predicate with `cleanup_tick`. (Subscribers hold a
                    // `broadcast::Receiver`, not an `Arc<UpstreamEntry>`, so
                    // a strong-count test would wrongly sweep live entries.)
                    // The sweep does not consume the poke grace.
                    map.retain(|_, ch| {
                        ch.monitors.retain(|_, e| e.is_retained());
                        !ch.monitors.is_empty() || ch.connection.is_some()
                    });
                }
                if is_new_channel && map.len() >= self.max_entries {
                    tracing::warn!(
                        pv = %pv_name,
                        len = map.len(),
                        cap = self.max_entries,
                        "pva-gateway: channel cache full, refusing new channel"
                    );
                    return Err(GwError::CacheFull(self.max_entries));
                }
                let fresh = self.spawn_upstream_monitor(pv_name, pv_request.cloned());
                map.entry(pv_name.to_string())
                    .or_insert_with(ChannelEntry::new)
                    .monitors
                    .insert(req_key.clone(), fresh.clone());
                (fresh, true)
            }
        };

        // Drop guard: removes the entry on early-exit (timeout OR
        // cancellation). Disarmed on success.
        struct CleanupGuard<'a> {
            cache: &'a ChannelCache,
            pv_name: &'a str,
            req_key: &'a [u8],
            armed: bool,
        }
        impl<'a> CleanupGuard<'a> {
            fn disarm(&mut self) {
                self.armed = false;
            }
        }
        impl<'a> Drop for CleanupGuard<'a> {
            fn drop(&mut self) {
                if !self.armed {
                    return;
                }
                // Remove the fresh-but-unconnected variant so a
                // cancellation race (caller's outer timeout / abort
                // dropping the future before await_first_event returns)
                // does not pin an upstream-monitor task. Pruning an emptied
                // channel is the `remove_variant` owner's job. A later
                // lookup re-probes from scratch.
                if let Ok(mut map) = self.cache.entries.try_lock() {
                    remove_variant(&mut map, self.pv_name, self.req_key);
                    return;
                }
                // Lock contended — spawn a tiny task that takes the
                // async lock and removes the orphan. Without this,
                // the orphan survives a full cleanup TTL because
                // cleanup_tick treats drop_poke=true (initial state)
                // as "recently used, keep".
                let entries = self.cache.entries.clone();
                let pv_name = self.pv_name.to_string();
                let req_key = self.req_key.to_vec();
                self.cache.reactor.spawn(async move {
                    let mut map = entries.lock().await;
                    remove_variant(&mut map, &pv_name, &req_key);
                });
            }
        }

        let mut guard = CleanupGuard {
            cache: self,
            pv_name,
            req_key: &req_key,
            armed: was_fresh,
        };
        match self.await_first_event(entry, connect_timeout).await {
            Ok(e) => {
                guard.disarm();
                Ok(e)
            }
            Err(e) => {
                // Guard fires on drop to remove the unconnected entry.
                // No negative-result record: the next lookup re-probes
                // upstream (pva2pva parity).
                Err(e)
            }
        }
    }

    /// Admit a channel by upstream CONNECTION only (no monitor), routing
    /// `has_pv` / `pvconnect` admission through the bounded gateway-owned
    /// cache. Connects through this cache's own client — the same pool the
    /// monitors and the follow-up GET/PUT/RPC reuse — then records a
    /// connection admission so the channel is counted in `cacheSize`,
    /// charged against the admission cap, surfaced in the operator status
    /// report, and reachable by `:drop`/`:flush`, instead of being a
    /// connectionless probe that left an untracked `client.inner.channels`
    /// entry. pva2pva answers `channelFind` from `ChannelCache::lookup`,
    /// which caches the connected channel (`chancache.cpp:166-199`) rather
    /// than bypassing the cache; the actual channel lifetime is still owned
    /// by the client's idle reaper. Returns the resolved upstream address.
    pub async fn admit_connection(
        &self,
        pv_name: &str,
        connect_timeout: Duration,
    ) -> GwResult<SocketAddr> {
        // Channel admission cap (mirrors `lookup_with_request`): only a
        // brand-new channel may be refused; pre-sweep idle channels the
        // cleaner would also evict before refusing.
        {
            let mut map = self.entries.lock().await;
            let is_new = !map.contains_key(pv_name);
            if is_new && map.len() >= self.max_entries {
                map.retain(|_, ch| {
                    ch.monitors.retain(|_, e| e.is_retained());
                    !ch.monitors.is_empty() || ch.connection.is_some()
                });
            }
            if !map.contains_key(pv_name) && map.len() >= self.max_entries {
                tracing::warn!(
                    pv = %pv_name,
                    len = map.len(),
                    cap = self.max_entries,
                    "pva-gateway: channel cache full, refusing connection admission"
                );
                return Err(GwError::CacheFull(self.max_entries));
            }
        }
        // Connect through the owned client (one-shot, bounded by the
        // caller's connect timeout exactly as the prior direct `pvconnect`
        // was). On failure nothing is recorded — the next probe re-resolves.
        let addr =
            epics_base_rs::runtime::task::timeout(connect_timeout, self.client.pvconnect(pv_name))
                .await
                .map_err(|_| GwError::UpstreamTimeout(pv_name.to_string()))??;
        // Record / refresh the connection admission (poked recent).
        self.entries
            .lock()
            .await
            .entry(pv_name.to_string())
            .or_insert_with(ChannelEntry::new)
            .connection = Some(true);
        Ok(addr)
    }

    /// Spawn an upstream monitor task and return a populated
    /// `UpstreamEntry`. The task writes directly into shared `Arc`s
    /// (state + first_event signal + broadcast sender) so the entry
    /// itself doesn't have to exist before the task is spawned.
    ///
    /// **Auto-restart**: `pvmonitor_typed` returns when the upstream
    /// channel ends (transient I/O, IOC restart). Without restart,
    /// the cache entry would happily serve a stale `snapshot()`
    /// forever (review §3a). We wrap the call in a backoff loop so a
    /// re-subscribe is attempted on every drop. When the backoff hits
    /// the configured ceiling without a successful subscribe AND
    /// nobody is listening anymore, the loop exits and the cleanup
    /// tick eventually evicts the orphan entry.
    fn spawn_upstream_monitor(
        &self,
        pv_name: &str,
        pv_request: Option<PvField>,
    ) -> Arc<UpstreamEntry> {
        let (tx, _rx0) = broadcast::channel::<MonitorUpdate>(BROADCAST_CAPACITY);
        let (tx_raw, _rx0_raw) =
            broadcast::channel::<crate::pva_gateway::source::RawEvent>(BROADCAST_CAPACITY);
        let first_event = Arc::new(Notify::new());
        let state = Arc::new(RwLock::new(EntryState::default()));

        let latest_raw = Arc::new(RwLock::new(None::<crate::pva_gateway::source::RawEvent>));

        let pv_name_owned = pv_name.to_string();
        let client = self.client.clone();
        let tx_for_task = tx.clone();
        let tx_raw_for_task = tx_raw.clone();
        let state_for_task = state.clone();
        let first_event_for_task = first_event.clone();
        let latest_raw_for_task = latest_raw.clone();
        // The downstream-forwarded pvRequest (if any) is re-cloned per
        // reconnect because each `pvmonitor_raw_frames_handle_with_request`
        // call consumes it; the client re-encodes it per upstream
        // connection.
        let pv_request_for_task = pv_request;

        let join = self.reactor.spawn(async move {
            let mut backoff = Duration::from_millis(250);
            let max_backoff = Duration::from_secs(30);
            // Whether the current subscription delivered any upstream event
            // before it ended. Set by `raw_cb` on the first frame, read +
            // reset after `handle.wait_terminal()` to decide whether to re-arm the
            // backoff floor (healthy connection that dropped) or keep the
            // grown backoff (a clean MONITOR FINISH that ended without ever
            // delivering an event must not tight-loop).
            let event_seen = Arc::new(AtomicBool::new(false));
            loop {
                let tx_inner = tx_for_task.clone();
                let state_inner = state_for_task.clone();
                let first_event_inner = first_event_for_task.clone();
                let _pv_name_for_cb = pv_name_owned.clone();

                // final form: TRUE wire-bytes forwarding via
                // `pvmonitor_raw_frames_handle` — the upstream monitor
                // task never decodes the value. The body bytes flow
                // straight from upstream socket → broadcast →
                // downstream socket. We only decode lazily when
                // `state.latest` is genuinely needed (the cache's
                // first-event signal + future typed `subscribe()`
                // callers, which today are unused for the gateway
                // path).
                let tx_raw_inner = tx_raw_for_task.clone();
                let pv_clone = pv_name_owned.clone();
                let latest_raw_inner = latest_raw_for_task.clone();
                let event_seen_inner = event_seen.clone();
                // tx_inner moves into the callback so decoded
                // events fan out to typed subscribers (subscribe_inner /
                // subscribe_checked fallback path). Pre-fix this sender
                // was dropped here before the closure captured it, so
                // bcast_rx.recv() in subscribe_inner blocked forever
                // after the initial snapshot.
                let raw_cb =
                    move |desc: &FieldDesc,
                          body: bytes::Bytes,
                          order: epics_pva_rs::proto::ByteOrder| {
                        // An upstream frame arrived: this subscription is
                        // delivering events, so the reconnect loop may re-arm
                        // its backoff floor when it ends (vs. a clean FINISH
                        // that returns without ever delivering one).
                        event_seen_inner.store(true, Ordering::Relaxed);
                        let outcome = apply_monitor_event(&state_inner, desc, &body, order);
                        use crate::pva_gateway::source::RawEvent;
                        if outcome.type_changed {
                            tracing::warn!(
                                pv = %pv_clone,
                                "pva-gateway: upstream introspection changed — \
                                 emitting type-change boundary to downstream monitors \
                                 (cache descriptor reset)"
                            );
                            // pvxs treats reconnect/type-change as
                            // a subscription boundary
                            // (ioc/pvalink_channel.cpp:342-351 `onTypeChange()`).
                            // Forwarding the new body under the downstream's
                            // original MONITOR INIT descriptor would deliver
                            // bytes the client can't decode — possibly causing
                            // a protocol error or silently corrupted values.
                            // Emit a marker event with no body so the
                            // downstream dispatch path sends MONITOR FINISH
                            // and the client knows to reopen with a fresh
                            // INIT against the new descriptor.
                            // clear stale bytes so new raw
                            // subscribers don't replay the old descriptor.
                            *latest_raw_inner.write() = None;
                            // Emit the boundary on BOTH fanout streams through
                            // the single owner: the raw `RawEvent` for raw-path
                            // subscribers AND the decoded
                            // `MonitorUpdate::type_change()` so a field-masked /
                            // pipelined / filtered downstream monitor — forced
                            // onto the decoded path — also gets a MONITOR FINISH
                            // instead of the next value re-encoded under its
                            // stale INIT descriptor.
                            broadcast_boundary(&tx_inner, &tx_raw_inner, order);
                            // Skip the normal body forward — the bytes are
                            // for the NEW descriptor; sending them under
                            // the old INIT descriptor is exactly the
                            // bug.
                            return;
                        }
                        if outcome.was_first {
                            first_event_inner.notify_waiters();
                        }
                        // fan out decoded value to typed subscribers
                        // (subscribe/subscribe_checked fallback path).
                        // Guard: skip the initial event (`was_first`) because
                        // subscribe_inner always delivers it via snapshot().
                        // Broadcasting it races with bcast_rx creation and
                        // produces a duplicate first value in the mpsc.
                        // `outcome.value` was read under the state write lock,
                        // so it is the exact merged value — no separate
                        // state_inner.read() re-acquisition needed.
                        if !outcome.was_first {
                            if let Some(val) = outcome.value {
                                // Carry the upstream event's real changed-leaf
                                // paths so the downstream cooked monitor's
                                // changedBitSet matches what the upstream IOC
                                // marked, not a full mask of every requested
                                // leaf (pva2pva moncache.cpp:142,187 copy the
                                // upstream changedBitSet verbatim). `overrun`
                                // is filled later by `subscribe_inner` on lag.
                                let _ = tx_inner.send(epics_pva_rs::server_native::MonitorUpdate {
                                    value: val,
                                    marked: outcome.marked,
                                    type_changed: false,
                                    overrun: Vec::new(),
                                });
                            }
                        }
                        // cache latest raw event for initial
                        // snapshot delivery to new raw subscribers.
                        // Clone is cheap (Bytes is refcounted).
                        let raw_ev = RawEvent {
                            body,
                            byte_order: order,
                            type_changed: false,
                        };
                        *latest_raw_inner.write() = Some(raw_ev.clone());
                        // Fan out raw body — refcount only, no copy.
                        let _ = tx_raw_inner.send(raw_ev);
                    };
                // The upstream monitor's CONNECTION-STATE stream — where
                // this task learns that the upstream came or went.
                //
                // It must not be inferred from the subscription handle
                // terminating: `spawn_raw_frames_handle` re-subscribes
                // INTERNALLY on `MonitorEnd::ConnectionLost` (announce, sleep
                // 200 ms, loop), so a plain upstream loss never makes the
                // handle's task return and the boundary below never fired —
                // downstream monitors kept being served the cached value of a
                // dead IOC at NoAlarm.
                // pva2pva surfaces a lost upstream as downstream *unlisten*
                // (`moncache.cpp:212-235`); `signal_disconnect_boundary` is
                // that unlisten and is idempotent per outage, so calling it
                // from both observers (here and after the terminal end
                // below) fires it exactly once — the same two-observer,
                // one-owner shape `inp_disconnect_scan` has in pvalink.
                let state_conn = state_for_task.clone();
                let latest_raw_conn = latest_raw_for_task.clone();
                let tx_conn = tx_for_task.clone();
                let tx_raw_conn = tx_raw_for_task.clone();
                let pv_name_conn = pv_name_owned.clone();
                let conn_cb = move |ev: epics_pva_rs::client_native::ops_v2::MonitorConnEvent| {
                    use epics_pva_rs::client_native::ops_v2::MonitorConnEvent;
                    match ev {
                        MonitorConnEvent::Connected { .. } => {}
                        MonitorConnEvent::Disconnected | MonitorConnEvent::Finished => {
                            tracing::debug!(
                                pv = %pv_name_conn,
                                "pva-gateway: upstream monitor left the connected state — \
                                 revoking the cached value with a monitor-unlisten boundary"
                            );
                            signal_disconnect_boundary(
                                &state_conn,
                                &latest_raw_conn,
                                &tx_conn,
                                &tx_raw_conn,
                            );
                        }
                    }
                };

                // Open the upstream monitor with the downstream's
                // forwarded pvRequest when one was captured (so the
                // upstream server applies the same field projection /
                // `record._options._filter` chain the client asked
                // for), else the default all-fields request. pva2pva
                // `p2pApp/channel.cpp:157-224` forwards the serialized
                // downstream pvRequest rather than a gateway default.
                let handle_result = match pv_request_for_task.clone() {
                    Some(req) => {
                        client
                            .pvmonitor_raw_frames_handle_with_request(
                                &pv_name_owned,
                                req,
                                raw_cb,
                                conn_cb,
                            )
                            .await
                    }
                    None => {
                        client
                            .pvmonitor_raw_frames_handle(&pv_name_owned, raw_cb, conn_cb)
                            .await
                    }
                };
                // `pvmonitor_raw_frames_handle*` returns immediately
                // with a handle whose internal task drives the
                // monitor loop; we wait for that task to terminate
                // (clean disconnect, channel close, or fatal error).
                // The handle's `pauser()` is deliberately unused: the
                // gateway never throttles a shared upstream (see the
                // invariant above the `UpstreamEntry` fields).
                let handle = match handle_result {
                    Ok(h) => h,
                    Err(e) => {
                        tracing::warn!(
                            pv = %pv_name_owned,
                            error = %e,
                            backoff_ms = backoff.as_millis() as u64,
                            "pva-gateway: raw upstream monitor failed to start, will retry"
                        );
                        // Upstream unreachable: revoke any cached value via a
                        // monitor-unlisten boundary so downstream monitors
                        // reopen instead of observing stale data at NoAlarm.
                        // Idempotent per outage (clears the snapshot), so a
                        // backoff retry storm emits FINISH at most once.
                        signal_disconnect_boundary(
                            &state_for_task,
                            &latest_raw_for_task,
                            &tx_for_task,
                            &tx_raw_for_task,
                        );
                        // guard removed — cleanup_tick aborts via AbortOnDrop.
                        epics_base_rs::runtime::task::sleep(backoff).await;
                        backoff = std::cmp::min(backoff * 2, max_backoff);
                        continue;
                    }
                };
                // Why the subscription ENDED — never why it disconnected.
                // `MonitorTermination` carries no connection state by
                // construction; the disconnect already reached `conn_cb`
                // above, whether or not the loop ended.
                let raw_result = handle.wait_terminal().await;
                // The subscription ended for good, which is also a departure
                // from the connected state: surface it to downstream PVA
                // monitors as a subscription boundary (MONITOR FINISH →
                // reopen), mirroring pva2pva moncache.cpp unlisten rather than
                // fabricating an INVALID alarm value. Idempotent per outage,
                // so this is a no-op when `conn_cb` already fired for the same
                // outage. The gateway's own loop re-subscribes; the first real
                // upstream event after reconnect repopulates the snapshot via
                // the normal monitor callback path.
                signal_disconnect_boundary(
                    &state_for_task,
                    &latest_raw_for_task,
                    &tx_for_task,
                    &tx_raw_for_task,
                );
                if let epics_pva_rs::client_native::ops_v2::MonitorTermination::Failed(e) =
                    raw_result
                {
                    tracing::warn!(
                        pv = %pv_name_owned,
                        error = %e,
                        backoff_ms = backoff.as_millis() as u64,
                        "pva-gateway: raw upstream monitor failed, will retry"
                    );
                }
                // Unconditional re-subscribe delay. The loop top re-issues
                // the upstream monitor; sleeping on EVERY iteration — clean
                // FINISH (Ok) and disconnect/error (Err) alike — is what
                // stops a clean MONITOR FINISH from tight-looping
                // INIT→FINISH→resubscribe with zero delay. The channel stays
                // Active after a FINISH (only the IOID is unregistered), so
                // without this a complete-then-finish PV spins at the CPU's
                // mercy. pva2pva treats unlisten as terminal
                // (moncache.cpp:214-236); we reconnect transparently but
                // never with zero delay, matching the ca_gateway and pvalink
                // sibling loops that always sleep before re-opening. A
                // subscription that delivered an event re-arms the floor for
                // a prompt reconnect; one that never delivered one keeps the
                // grown backoff (see `resubscribe_backoff`).
                let (delay, next_backoff) = resubscribe_backoff(
                    event_seen.swap(false, Ordering::Relaxed),
                    backoff,
                    Duration::from_millis(250),
                    max_backoff,
                );
                epics_base_rs::runtime::task::sleep(delay).await;
                backoff = next_backoff;
                // guard removed — cleanup_tick aborts via AbortOnDrop.

                // Both typed (PvField) and raw-frame channels feed
                // downstreams; raw-forwarding is default-on so
                // most production subscribers ride tx_raw and tx is
                // empty. Only exit when BOTH have no live receivers,
                // otherwise upstream IOC restart silently kills every
                // raw-path downstream monitor.
                // guard removed — cleanup_tick evicts idle entries
                // (subscriber_count==0 && !drop_poke) and aborts this task
                // via AbortOnDrop. Keeping the task alive until eviction
                // prevents new subscribers from joining a dead broadcast.
            }
        });

        Arc::new(UpstreamEntry {
            pv_name: pv_name.to_string(),
            state,
            tx,
            tx_raw,
            latest_raw,
            first_event,
            _monitor_task: AbortOnDrop(join.abort_handle()),
            drop_poke: parking_lot::Mutex::new(true),
        })
    }

    /// Wait on `entry.first_event` (with `connect_timeout`) for the
    /// upstream monitor to deliver its first frame. Returns the entry
    /// once populated, or `GwError::UpstreamTimeout` on deadline.
    ///
    /// Race-safe: pins `notified()` before checking the snapshot,
    /// so a value that lands between the snapshot check and the
    /// await is still observed. (`tokio::sync::Notify` only delivers
    /// to waiters created before `notify_waiters`.)
    async fn await_first_event(
        &self,
        entry: Arc<UpstreamEntry>,
        connect_timeout: Duration,
    ) -> GwResult<Arc<UpstreamEntry>> {
        // Hold the Notify Arc separately so we can `notified()` it
        // without borrowing `entry` (which we'd return below).
        let notify = entry.first_event.clone();
        let notified = notify.notified();
        // Pin so subsequent notify_waiters() wakes us.
        tokio::pin!(notified);
        if entry.snapshot().is_some() {
            return Ok(entry);
        }
        let res = epics_base_rs::runtime::task::timeout(connect_timeout, &mut notified).await;
        if res.is_err() && entry.snapshot().is_none() {
            return Err(GwError::UpstreamTimeout(entry.pv_name.clone()));
        }
        Ok(entry)
    }

    /// Remove every entry that hasn't been touched since the previous
    /// cleanup tick AND has zero downstream subscribers. Mirrors p2pApp
    /// `cacheClean::expire`.
    async fn cleanup_tick(&self) {
        let mut map = self.entries.lock().await;
        let before = map.len();
        map.retain(|_, channel| {
            // Evict idle monitor variants within the channel, then drop the
            // channel itself once it holds no variant. cacheSize / the
            // cleaner counter are channel-level, so an evicted variant of a
            // still-busy channel does not count as a removed channel.
            channel.monitors.retain(|_, entry| {
                // Same keep-predicate as the cache-full emergency sweep.
                let retained = entry.is_retained();
                if retained {
                    // Consume one tick of `drop_poke` grace (pva2pva resets
                    // `dropPoke` on keep, `chancache.cpp:126`), so a
                    // poked-but-idle variant is evicted on the next tick once
                    // it has no subscribers. Harmless on a subscriber-
                    // retained variant whose poke may already be false.
                    *entry.drop_poke.lock() = false;
                }
                retained
            });
            // Age out a connection-only admission the same one-tick way:
            // `Some(true)` consumes its grace, `Some(false)` evicts the
            // marker. A channel with live monitors is retained regardless;
            // a connection-only channel survives exactly one idle tick.
            let connection_retained = match channel.connection {
                Some(true) => {
                    channel.connection = Some(false);
                    true
                }
                Some(false) => {
                    channel.connection = None;
                    false
                }
                None => false,
            };
            !channel.monitors.is_empty() || connection_retained
        });
        // pva2pva bumps `cleanerRuns` every sweep and `cleanerDust` by the
        // number of evicted CHANNELS (`chancache.cpp:230-262`,
        // `p2pApp/server.cpp:182-198`); both surface in the operator
        // status report.
        // Relaxed is sufficient — these are monotonic diagnostic counters
        // with no ordering dependency.
        self.cleaner_runs.fetch_add(1, Ordering::Relaxed);
        self.cleaner_removed
            .fetch_add((before - map.len()) as u64, Ordering::Relaxed);
    }

    /// Snapshot of cached PV names — used by `ChannelSource::list_pvs`.
    /// The top-level cache is keyed by channel name (one entry per PV
    /// regardless of how many pvRequest variants it holds), so the keys are
    /// already unique; just sort for a stable listing.
    pub async fn names(&self) -> Vec<String> {
        let map = self.entries.lock().await;
        let mut names: Vec<String> = map.keys().cloned().collect();
        drop(map);
        names.sort_unstable();
        names
    }

    /// Diagnostic: total cached channels (one per PV name, irrespective of
    /// how many pvRequest variants each holds) — pva2pva `entries.size()`.
    pub async fn entry_count(&self) -> usize {
        self.entries.lock().await.len()
    }

    /// Configured hard cap on cached entries for this cache. Exposed so
    /// the gateway can verify that per-credential caches inherit the
    /// configured policy rather than a hardcoded default,
    /// and for control-status reporting.
    pub fn max_entries(&self) -> usize {
        self.max_entries
    }

    /// Number of cleanup-tick sweeps run so far (pva2pva `cleanerRuns`).
    pub fn cleaner_runs(&self) -> u64 {
        self.cleaner_runs.load(Ordering::Relaxed)
    }

    /// Total entries evicted by the cleanup tick (pva2pva `cleanerDust`).
    pub fn cleaner_removed(&self) -> u64 {
        self.cleaner_removed.load(Ordering::Relaxed)
    }

    /// Per-entry status snapshot for the control status report.
    /// Mirrors the per-channel detail
    /// pva2pva's `status_client` emits at verbose levels
    /// (`p2pApp/server.cpp:203-230`): upstream connection state,
    /// downstream subscriber count, and the idle-eviction grace bit.
    /// `limit` caps the returned rows so a 50 k-entry cache cannot
    /// produce a multi-megabyte report PV; the returned `truncated`
    /// flag reports how many rows were dropped.
    pub async fn entry_status(&self, limit: usize) -> CacheStatus {
        let map = self.entries.lock().await;
        let total = map.len();
        let mut entries: Vec<EntryStatus> = Vec::with_capacity(total.min(limit));
        for (pv_name, channel) in map.iter().take(limit) {
            entries.push(EntryStatus {
                pv_name: pv_name.clone(),
                // Channel-level aggregates over the monitor variants — pva2pva
                // emits one row per channel with its `<n>` unique
                // subscriptions (`p2pApp/server.cpp:218,228`), not one row per
                // variant.
                connected: channel.connected(),
                subscribers: channel.subscriber_count(),
                subscriptions: channel.monitors.len(),
                drop_poke: channel.drop_poke(),
            });
        }
        CacheStatus {
            total,
            truncated: total.saturating_sub(entries.len()),
            entries,
            cleaner_runs: self.cleaner_runs(),
            cleaner_removed: self.cleaner_removed(),
            max_entries: self.max_entries,
        }
    }

    /// Wire the server-wide channel invalidator (see the
    /// `invalidator` field). Called by the bound
    /// `GatewayChannelSource` when the native server hands it the handle.
    /// Idempotent — a second set is ignored (`OnceLock`), so
    /// re-registration of the shared cache on `<prefix>:reload` is
    /// harmless.
    pub fn set_invalidator(&self, invalidator: ChannelInvalidator) {
        let _ = self.invalidator.set(invalidator);
    }

    /// Publish the PV names removed by an operator-driven [`Self::flush`] /
    /// [`Self::drop_entry`] onto the server-wide invalidator as ONE batch, so
    /// every per-connection task force-disconnects the matching downstream
    /// channels (the downstream effect of dropping the cache entry, pva2pva
    /// `channel->destroy()` fanout). One command = one batch regardless of how
    /// many entries it removed, and the per-connection queues are unbounded,
    /// so no name is ever dropped even on a full-cache `:flush`.
    /// No-op when the cache is not wired to a server
    /// (standalone caches in tests) or when the batch is empty. Only
    /// operator-driven removal publishes: the idle cleanup tick evicts entries
    /// that have no downstream interest, so disconnecting on eviction would
    /// needlessly kill idle-but-open channels and defeat the cache.
    fn publish_invalidation(&self, names: impl IntoIterator<Item = String>) {
        let Some(inv) = self.invalidator.get() else {
            return;
        };
        let batch: Arc<[String]> = names.into_iter().collect();
        inv.publish(batch);
    }

    /// B6: operator-driven cache flush. Drops every cached channel (and
    /// with it every nested `UpstreamEntry`), then returns the number of
    /// channels removed.
    ///
    /// Each removed entry's `AbortOnDrop` aborts its upstream monitor
    /// task once the last `Arc<UpstreamEntry>` is released; any live
    /// downstream subscriber holding a `broadcast::Receiver` simply
    /// stops receiving events (the next downstream search re-opens a
    /// fresh upstream monitor). This mirrors `pva2pva`'s manual
    /// channel-cache drop.
    ///
    /// As the single removal owner, flush also publishes each
    /// dropped PV name on the channel-invalidation stream so every
    /// downstream channel bound to a dropped entry is force-disconnected,
    /// not merely starved of events. pva2pva drops the `ChannelCacheEntry`
    /// and `channel->destroy()` fans `DESTROYED` to every `GWChannel`
    /// (chancache.cpp:76-99); previously a Rust downstream channel stayed
    /// open and silently bound to a re-created entry on the next event.
    pub async fn flush(&self) -> usize {
        let (removed, names) = {
            let mut map = self.entries.lock().await;
            let removed = map.len();
            // The cache is keyed by channel name, so the keys are already
            // one invalidation per downstream channel.
            let names: Vec<String> = map.keys().cloned().collect();
            map.clear();
            (removed, names)
        };
        // Publish after releasing the entries lock.
        self.publish_invalidation(names);
        removed
    }

    /// B6: drop the cached channel for an exact PV name, taking all its
    /// pvRequest monitor variants with it (an operator-driven drop targets
    /// the channel, so every variant under it goes). Returns `true` if the
    /// channel was present and removed.
    ///
    /// On a removal, also publishes the name on the
    /// channel-invalidation stream so the matching downstream channel is
    /// force-disconnected (same rationale as [`Self::flush`]).
    pub async fn drop_entry(&self, pv_name: &str) -> bool {
        let removed = self.entries.lock().await.remove(pv_name).is_some();
        if removed {
            self.publish_invalidation(std::iter::once(pv_name.to_string()));
        }
        removed
    }

    /// Test-only: insert a synthetic, parked entry under `pv_name` so
    /// cache-administration paths (`entry_count` / `flush` /
    /// `drop_entry`, and the gateway's all-layers
    /// aggregation across shared + per-credential caches) can be
    /// exercised without a live upstream IOC.
    #[cfg(test)]
    pub(crate) async fn insert_test_entry(&self, pv_name: &str) {
        // Default-fanout variant (empty pvRequest key).
        self.insert_test_variant(pv_name, Vec::new()).await;
    }

    /// Test-only: insert a synthetic, parked monitor variant for `pv_name`
    /// under the serialized-pvRequest key `req_key`, creating the channel
    /// entry if absent. Lets accounting tests build a single channel with
    /// several pvRequest variants without a live upstream IOC.
    #[cfg(test)]
    pub(crate) async fn insert_test_variant(&self, pv_name: &str, req_key: Vec<u8>) {
        let (tx, rx0) = broadcast::channel::<MonitorUpdate>(4);
        drop(rx0);
        let (tx_raw, rx0_raw) = broadcast::channel::<crate::pva_gateway::source::RawEvent>(4);
        drop(rx0_raw);
        let task = self.reactor.spawn(std::future::pending::<()>());
        let entry = Arc::new(UpstreamEntry {
            pv_name: pv_name.to_string(),
            state: Arc::new(RwLock::new(EntryState::default())),
            tx,
            tx_raw,
            latest_raw: Arc::new(RwLock::new(None)),
            first_event: Arc::new(Notify::new()),
            _monitor_task: AbortOnDrop(task.abort_handle()),
            drop_poke: parking_lot::Mutex::new(false),
        });
        // Pre-mark the parked variant connected: store a `first_event`
        // permit so a `lookup_with_request` resolves it immediately
        // (`await_first_event` consumes the permit) without a live upstream
        // IOC ever firing the signal.
        entry.first_event.notify_one();
        self.entries
            .lock()
            .await
            .entry(pv_name.to_string())
            .or_insert_with(ChannelEntry::new)
            .monitors
            .insert(req_key, entry);
    }

    /// Test-only: insert a synthetic connection-only admission (no monitor
    /// variant) for `pv_name`, as `admit_connection` would after a
    /// successful connect — lets accounting tests exercise cacheSize /
    /// cleanup grace / drop without a live upstream IOC.
    #[cfg(test)]
    pub(crate) async fn insert_test_connection(&self, pv_name: &str) {
        let mut map = self.entries.lock().await;
        map.entry(pv_name.to_string())
            .or_insert_with(ChannelEntry::new)
            .connection = Some(true);
    }

    /// Test-only: run one cleanup sweep (the periodic cleaner's body) so a
    /// test can drive idle eviction deterministically.
    #[cfg(test)]
    pub(crate) async fn cleanup_tick_for_test(&self) {
        self.cleanup_tick().await;
    }

    /// Test-only: publish one decoded value on `pv_name`'s default variant the
    /// way the upstream monitor callback does — FOLD it into the entry's
    /// `latest` (and its introspection), then fan it out. Pushing straight into
    /// the broadcast sender instead would leave `latest` empty — a state no live
    /// upstream can be in once it has delivered a value, and one in which there
    /// is no merged snapshot to re-frame a lagged subscriber from
    /// (`source::reframe_raw_body_after_lag`).
    #[cfg(test)]
    pub(crate) async fn test_publish(&self, pv_name: &str, value: PvField) {
        let entry = self
            .entries
            .lock()
            .await
            .get(pv_name)
            .and_then(|ch| ch.monitors.get(&Vec::new()).cloned())
            .expect("default test monitor variant must exist");
        {
            let mut s = entry.state.write();
            s.introspection = Some(value.descriptor());
            s.latest = Some(value.clone());
        }
        let _ = entry.tx.send(MonitorUpdate::from(value));
    }
}

impl Drop for ChannelCache {
    fn drop(&mut self) {
        if let Some(task) = self.cleanup_task.lock().take() {
            task.abort();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use epics_pva_rs::proto::{BitSet, ByteOrder};
    use epics_pva_rs::pvdata::{ScalarType, ScalarValue};

    /// The cached snapshot's merged value. What a monitor seed built from it
    /// declares on the wire is asserted by
    /// [`r18_28_monitor_seed_declares_the_whole_structure`].
    fn latest_value(state: &RwLock<EntryState>) -> Option<PvField> {
        state.read().latest.clone()
    }

    /// The reconnect loop's re-subscribe delay must be applied on EVERY
    /// path — a clean MONITOR FINISH (Ok) as well as an error (Err) — so a
    /// completing upstream PV cannot tight-loop INIT→FINISH→resubscribe with
    /// zero delay (the BRPVAGW-1 busy-loop). Pre-fix the Ok path slept 0 and
    /// reset the floor unconditionally. By invariant boundary:
    /// - delivered an event → re-arm the floor (prompt reconnect);
    /// - no event, grown backoff → keep it and STILL sleep it (never 0);
    /// - doubling saturates at `max`.
    #[test]
    fn resubscribe_backoff_never_zero_and_floors_only_after_event() {
        let floor = Duration::from_millis(250);
        let max = Duration::from_secs(30);

        // Delivered an event then dropped (healthy): a grown backoff
        // collapses back to the floor for a prompt reconnect.
        let (delay, next) = resubscribe_backoff(true, Duration::from_secs(8), floor, max);
        assert_eq!(delay, floor, "event-bearing subscription re-arms the floor");
        assert_eq!(
            next,
            Duration::from_millis(500),
            "next backoff doubles the floor"
        );

        // Clean FINISH with NO event: keep the grown backoff and still sleep
        // it. This is the busy-loop fix — the delay is never zero.
        let (delay, next) = resubscribe_backoff(false, Duration::from_secs(4), floor, max);
        assert_eq!(
            delay,
            Duration::from_secs(4),
            "no-event end keeps the grown backoff"
        );
        assert_ne!(
            delay,
            Duration::ZERO,
            "a clean FINISH must never re-subscribe with zero delay"
        );
        assert_eq!(next, Duration::from_secs(8), "backoff doubles toward max");

        // First end with no event still sleeps the floor (> 0), not zero.
        let (delay, _next) = resubscribe_backoff(false, floor, floor, max);
        assert_eq!(delay, floor);
        assert!(delay > Duration::ZERO);

        // Doubling saturates at `max`.
        let (delay, next) = resubscribe_backoff(false, Duration::from_secs(20), floor, max);
        assert_eq!(delay, Duration::from_secs(20));
        assert_eq!(next, max, "next backoff saturates at max");
    }

    /// Encode a wire monitor body (`changed | value`) for `value`
    /// against `desc`, marking the bits in `set_bits` as changed.
    /// Mirrors what `pvmonitor_raw_frames_handle` hands the gateway
    /// callback (the trailing overrun bitset is not consumed by the
    /// decoder so it is omitted).
    fn encode_body(desc: &FieldDesc, value: &PvField, set_bits: &[usize]) -> Vec<u8> {
        let mut changed = BitSet::new();
        for &b in set_bits {
            changed.set(b);
        }
        let mut body = Vec::new();
        changed.write_into(ByteOrder::Little, &mut body);
        epics_pva_rs::pvdata::encode::encode_pv_field_with_bitset(
            value,
            desc,
            &changed,
            0,
            ByteOrder::Little,
            &mut body,
        );
        body
    }

    /// BUG 2 regression: `apply_monitor_event` must update
    /// `state.latest` on EVERY upstream monitor event, so a gateway
    /// GET (`UpstreamEntry::snapshot` → `get_value`) returns the live
    /// value. Pre-fix only the first event was decoded — the snapshot
    /// froze at the first value forever.
    #[test]
    fn bug2_get_value_tracks_every_monitor_event() {
        let desc = FieldDesc::Scalar(ScalarType::Double);
        let state = RwLock::new(EntryState::default());

        // First event: value = 1.0.
        let body1 = encode_body(&desc, &PvField::Scalar(ScalarValue::Double(1.0)), &[0]);
        let o1 = apply_monitor_event(&state, &desc, &body1, ByteOrder::Little);
        assert!(o1.was_first && o1.value.is_some() && !o1.type_changed);
        assert_eq!(
            latest_value(&state),
            Some(PvField::Scalar(ScalarValue::Double(1.0)))
        );

        // Second event: value = 2.0. Pre-fix this was DROPPED — the
        // snapshot would still read 1.0.
        let body2 = encode_body(&desc, &PvField::Scalar(ScalarValue::Double(2.0)), &[0]);
        apply_monitor_event(&state, &desc, &body2, ByteOrder::Little);
        assert_eq!(
            latest_value(&state),
            Some(PvField::Scalar(ScalarValue::Double(2.0))),
            "snapshot must reflect the 2nd monitor event, not freeze at the 1st"
        );

        // Third event: value = 3.0.
        let body3 = encode_body(&desc, &PvField::Scalar(ScalarValue::Double(3.0)), &[0]);
        apply_monitor_event(&state, &desc, &body3, ByteOrder::Little);
        assert_eq!(
            latest_value(&state),
            Some(PvField::Scalar(ScalarValue::Double(3.0))),
            "snapshot must track the live value across many events"
        );
    }

    /// BRPVAGW-4 regression: a monitor event whose body fails to decode
    /// still counts as "first event arrived". C marks `havedata = true`
    /// unconditionally at the top of `MonitorCacheEntry::monitorEvent`
    /// (moncache.cpp:132-133), before any value copy, so the
    /// first-event/`havedata` signal does not depend on a successful decode.
    /// Pre-fix the decode-failure path returned `MonitorEventOutcome::
    /// default()` (was_first=false), so a malformed FIRST frame never fired
    /// `first_event` and `await_first_event` timed out and evicted a
    /// connectable PV as not-found.
    #[test]
    fn brpvagw4_decode_failure_first_event_still_signals_arrival() {
        let desc = FieldDesc::Scalar(ScalarType::Double);
        let state = RwLock::new(EntryState::default());

        // An empty body fails `BitSet::decode` (size byte short-reads) →
        // the decode-failure path.
        let outcome = apply_monitor_event(&state, &desc, &[], ByteOrder::Little);
        assert!(
            outcome.was_first,
            "a decode failure on the FIRST event must still report was_first \
             (C havedata=true) so first_event fires and the entry is not evicted"
        );
        assert!(
            outcome.value.is_none(),
            "no decoded value to report on failure"
        );
        assert!(!outcome.type_changed, "decode failure is not a type change");
        assert!(
            state.read().latest.is_none(),
            "decode failure must not fabricate a snapshot value"
        );

        // The failure path does NOT set introspection, so the first
        // DECODABLE event is still treated as the first and establishes the
        // real snapshot.
        let body = encode_body(&desc, &PvField::Scalar(ScalarValue::Double(7.0)), &[0]);
        let o2 = apply_monitor_event(&state, &desc, &body, ByteOrder::Little);
        assert!(
            o2.was_first,
            "the first successfully-decoded event after a failed one is still first"
        );
        assert_eq!(
            latest_value(&state),
            Some(PvField::Scalar(ScalarValue::Double(7.0)))
        );
    }

    /// BUG 2 / BUG 3 regression: a delta monitor event (only some
    /// fields marked changed) must be MERGED onto the prior snapshot,
    /// so unmarked fields keep their current value rather than being
    /// zero-filled. This is what makes the gateway's default
    /// `put_delta_checked` (get_value → fill_unmarked_from_prior →
    /// pvput) merge against CURRENT upstream data.
    #[test]
    fn bug2_delta_event_merges_onto_prior_snapshot() {
        // A 2-field structure: bit 0 = struct, 1 = a, 2 = b.
        let desc = FieldDesc::Structure {
            struct_id: String::new(),
            fields: vec![
                ("a".to_string(), FieldDesc::Scalar(ScalarType::Int)),
                ("b".to_string(), FieldDesc::Scalar(ScalarType::Int)),
            ],
        };
        let full = |a: i32, b: i32| {
            let mut s = epics_pva_rs::pvdata::PvStructure::new("");
            s.fields
                .push(("a".to_string(), PvField::Scalar(ScalarValue::Int(a))));
            s.fields
                .push(("b".to_string(), PvField::Scalar(ScalarValue::Int(b))));
            PvField::Structure(s)
        };
        let state = RwLock::new(EntryState::default());

        // First event: full value a=10, b=20 (whole struct marked).
        let body1 = encode_body(&desc, &full(10, 20), &[0, 1, 2]);
        apply_monitor_event(&state, &desc, &body1, ByteOrder::Little);
        assert_eq!(latest_value(&state), Some(full(10, 20)));

        // Delta event: only `a` changed → bit 1. `b` is unmarked and
        // arrives zero-filled; the merge must restore b=20.
        let body2 = encode_body(&desc, &full(99, 0), &[1]);
        apply_monitor_event(&state, &desc, &body2, ByteOrder::Little);
        assert_eq!(
            latest_value(&state),
            Some(full(99, 20)),
            "delta merge must keep unmarked field `b` at its prior value"
        );
    }

    /// BR-2 regression: `apply_monitor_event` must report the upstream
    /// event's real changed-leaf paths in `outcome.marked`, so the
    /// downstream cooked monitor advertises the upstream changedBitSet
    /// (pva2pva `p2pApp/moncache.cpp:142,187` copy it verbatim) instead of
    /// a full mask of every requested leaf. A whole-structure (root-bit)
    /// event reports `None` (= full mask); a delta reports exactly its
    /// changed leaves.
    #[test]
    fn br2_apply_monitor_event_reports_upstream_changed_leaves() {
        let desc = FieldDesc::Structure {
            struct_id: String::new(),
            fields: vec![
                ("a".to_string(), FieldDesc::Scalar(ScalarType::Int)),
                ("b".to_string(), FieldDesc::Scalar(ScalarType::Int)),
            ],
        };
        let full = |a: i32, b: i32| {
            let mut s = epics_pva_rs::pvdata::PvStructure::new("");
            s.fields
                .push(("a".to_string(), PvField::Scalar(ScalarValue::Int(a))));
            s.fields
                .push(("b".to_string(), PvField::Scalar(ScalarValue::Int(b))));
            PvField::Structure(s)
        };
        let state = RwLock::new(EntryState::default());

        // First event: whole struct (root bit 0). The whole structure
        // changed → `marked` None (full mask), matching pva2pva's first
        // element carrying the full changedBitSet.
        let body1 = encode_body(&desc, &full(10, 20), &[0, 1, 2]);
        let o1 = apply_monitor_event(&state, &desc, &body1, ByteOrder::Little);
        assert!(
            o1.marked.is_none(),
            "root-bit event reports the whole structure as a full mask"
        );

        // Delta: only `a` changed (bit 1) → marks exactly `a`, NOT a full
        // mask. This is the BR-2 fix: pre-fix every delta fanned out
        // marked:None and the server synthesised an all-leaves bitset.
        let body2 = encode_body(&desc, &full(99, 0), &[1]);
        let o2 = apply_monitor_event(&state, &desc, &body2, ByteOrder::Little);
        assert_eq!(
            o2.marked,
            Some(vec!["a".to_string()]),
            "a single-leaf delta marks only the leaf the upstream changed"
        );

        // Delta: only `b` changed (bit 2) → marks exactly `b`.
        let body3 = encode_body(&desc, &full(0, 77), &[2]);
        let o3 = apply_monitor_event(&state, &desc, &body3, ByteOrder::Little);
        assert_eq!(o3.marked, Some(vec!["b".to_string()]));
    }

    /// R18-28 regression: the monitor SEED declares the whole structure,
    /// whatever the upstream events that built the snapshot marked. pva2pva
    /// hands a starting MonitorUser the merged element with the ROOT bit set
    /// (`moncache.cpp:304-312`, "indicate all changed"); it does NOT replay
    /// the union of the upstream changed bitsets — that rule (`:142`) belongs
    /// to the update path. The R17-32 fix took the update rule for the seed
    /// rule and shipped seeds that could omit `alarm`/`timeStamp`.
    ///
    /// Boundaries: no upstream event (no seed at all); a partially-marked
    /// first event; a delta on top of it; a root-bit event.
    #[test]
    fn r18_28_monitor_seed_declares_the_whole_structure() {
        let desc = FieldDesc::Structure {
            struct_id: String::new(),
            fields: vec![
                ("a".to_string(), FieldDesc::Scalar(ScalarType::Int)),
                ("b".to_string(), FieldDesc::Scalar(ScalarType::Int)),
            ],
        };
        let full = |a: i32, b: i32| {
            let mut s = epics_pva_rs::pvdata::PvStructure::new("");
            s.fields
                .push(("a".to_string(), PvField::Scalar(ScalarValue::Int(a))));
            s.fields
                .push(("b".to_string(), PvField::Scalar(ScalarValue::Int(b))));
            PvField::Structure(s)
        };
        let state = RwLock::new(EntryState::default());

        // No upstream event yet: pva2pva's `!havedata` — no seed element at
        // all, so the downstream monitor's first frame is the first update.
        assert!(monitor_seed(&state).is_none());

        // First event marks only `a`. The seed still declares the whole
        // structure: pva2pva sets the root bit on the merged element.
        apply_monitor_event(
            &state,
            &desc,
            &encode_body(&desc, &full(10, 0), &[1]),
            ByteOrder::Little,
        );
        let seed = monitor_seed(&state).expect("an upstream event ⟹ a seed");
        assert_eq!(seed.value, full(10, 0));
        assert!(
            seed.marked.is_none(),
            "a seed is whole-structure (root bit), not the upstream's marks"
        );

        // A `b` delta merges onto the snapshot; the seed is still whole.
        apply_monitor_event(
            &state,
            &desc,
            &encode_body(&desc, &full(0, 20), &[2]),
            ByteOrder::Little,
        );
        let seed = monitor_seed(&state).expect("an upstream event ⟹ a seed");
        assert_eq!(seed.value, full(10, 20));
        assert!(seed.marked.is_none());

        // A root-bit event — the same seed, no special case.
        apply_monitor_event(
            &state,
            &desc,
            &encode_body(&desc, &full(1, 2), &[0, 1, 2]),
            ByteOrder::Little,
        );
        let seed = monitor_seed(&state).expect("an upstream event ⟹ a seed");
        assert_eq!(seed.value, full(1, 2));
        assert!(seed.marked.is_none());
    }

    /// `apply_monitor_event` flags a descriptor change so
    /// the gateway loop can emit a type-change marker event instead
    /// of forwarding the now-mismatched body. Prior code logged a
    /// warning and forwarded the bytes anyway; the downstream
    /// client decoded garbage under its stale INIT descriptor.
    #[test]
    fn br_r42_apply_monitor_event_flags_descriptor_change() {
        // Start: scalar double, value 1.0.
        let desc1 = FieldDesc::Scalar(ScalarType::Double);
        let state = RwLock::new(EntryState::default());
        let body1 = encode_body(&desc1, &PvField::Scalar(ScalarValue::Double(1.0)), &[0]);
        let o1 = apply_monitor_event(&state, &desc1, &body1, ByteOrder::Little);
        assert!(
            o1.was_first && !o1.type_changed,
            "first event must NOT report type_changed (no prior descriptor to compare)"
        );

        // Same descriptor, new value — type_changed must stay false.
        let body2 = encode_body(&desc1, &PvField::Scalar(ScalarValue::Double(2.0)), &[0]);
        let o2 = apply_monitor_event(&state, &desc1, &body2, ByteOrder::Little);
        assert!(
            !o2.was_first && !o2.type_changed,
            "same-descriptor event must NOT flag type_changed"
        );

        // Now upstream reconnects with a different shape — Int
        // instead of Double. The body bytes are encoded for `desc2`,
        // so the apply_monitor_event call MUST flag `type_changed=true`
        // so the gateway loop suppresses fan-out under the old INIT
        // descriptor and emits the marker event instead.
        let desc2 = FieldDesc::Scalar(ScalarType::Int);
        let body3 = encode_body(&desc2, &PvField::Scalar(ScalarValue::Int(42)), &[0]);
        let o3 = apply_monitor_event(&state, &desc2, &body3, ByteOrder::Little);
        assert!(
            o3.type_changed,
            "introspection change must be flagged for the marker path"
        );
        assert!(
            o3.value.is_some(),
            "the new-descriptor body still decodes cleanly"
        );
    }

    /// Smoke test: we can build an entry standalone (no cache, no
    /// real client) and exercise the subscribe / poke counters.
    #[tokio::test]
    async fn entry_subscribe_returns_fresh_receivers() {
        let (tx, rx0) = broadcast::channel::<MonitorUpdate>(4);
        drop(rx0);
        let (tx_raw, rx0_raw) = broadcast::channel::<crate::pva_gateway::source::RawEvent>(4);
        drop(rx0_raw);
        let task = crate::test_reactor().spawn(async {
            epics_base_rs::runtime::task::sleep(Duration::from_secs(60)).await;
        });
        let entry = UpstreamEntry {
            pv_name: "X".into(),
            state: Arc::new(RwLock::new(EntryState::default())),
            tx,
            tx_raw,
            latest_raw: Arc::new(RwLock::new(None)),
            first_event: Arc::new(Notify::new()),
            _monitor_task: AbortOnDrop(task.abort_handle()),
            drop_poke: parking_lot::Mutex::new(false),
        };
        assert_eq!(entry.subscriber_count(), 0);
        let _r1 = entry.subscribe();
        let _r2 = entry.subscribe();
        assert_eq!(entry.subscriber_count(), 2);
        assert!(*entry.drop_poke.lock(), "subscribe must poke");
    }

    /// both eviction paths share `is_retained`. Boundaries — idle &
    /// unsubscribed → evictable; poked → kept (one-tick grace); subscribed
    /// → kept even when `drop_poke == false` and `Arc::strong_count == 1`
    /// (a subscriber holds only a `broadcast::Receiver`). The old
    /// `strong_count > 1` sweep failed the last case and killed live
    /// upstream monitors.
    #[tokio::test]
    async fn is_retained_keeps_subscribers_and_poked_evicts_idle() {
        fn make(drop_poke: bool) -> UpstreamEntry {
            let (tx, rx0) = broadcast::channel::<MonitorUpdate>(4);
            drop(rx0);
            let (tx_raw, rx0_raw) = broadcast::channel::<crate::pva_gateway::source::RawEvent>(4);
            drop(rx0_raw);
            let task = crate::test_reactor().spawn(std::future::pending::<()>());
            UpstreamEntry {
                pv_name: "X".into(),
                state: Arc::new(RwLock::new(EntryState::default())),
                tx,
                tx_raw,
                latest_raw: Arc::new(RwLock::new(None)),
                first_event: Arc::new(Notify::new()),
                _monitor_task: AbortOnDrop(task.abort_handle()),
                drop_poke: parking_lot::Mutex::new(drop_poke),
            }
        }

        // Idle: no subscriber, no poke grace → evictable.
        let idle = make(false);
        assert!(
            !idle.is_retained(),
            "idle unsubscribed entry must be evictable"
        );

        // Poked but no subscriber → kept for one tick.
        let poked = make(true);
        assert!(poked.is_retained(), "poked entry keeps its grace");

        // Subscribed via broadcast Receiver, poke cleared to isolate: the
        // entry has strong_count == 1 (only the test holds the Arc-free
        // value) yet must be retained because a downstream is listening.
        let subbed = make(false);
        let _rx = subbed.subscribe(); // subscribe() also pokes…
        *subbed.drop_poke.lock() = false; // …clear it to prove subscriber-only retention.
        assert_eq!(subbed.subscriber_count(), 1);
        assert!(
            subbed.is_retained(),
            "subscribed entry must survive the cache-full sweep"
        );
    }

    /// On upstream disconnect the gateway emits a monitor-unlisten boundary
    /// (empty `type_changed` marker) and clears the cached snapshot — it
    /// does NOT fabricate an INVALID alarm value. Mirrors pva2pva
    /// `moncache.cpp:212-235` (lost upstream → downstream FINISH). The
    /// boundary MUST land on BOTH the raw and the decoded fanout streams: a
    /// field-masked / pipelined downstream monitor rides the decoded stream
    /// and would otherwise miss the disconnect boundary (the defect this
    /// closes, on the disconnect arm).
    #[test]
    fn disconnect_emits_unlisten_boundary_and_clears_snapshot() {
        use crate::pva_gateway::source::RawEvent;
        use tokio::sync::broadcast;
        let state = RwLock::new(EntryState::default());
        state.write().latest = Some(PvField::Scalar(ScalarValue::Double(1.0)));
        let latest_raw = RwLock::new(Some(RawEvent {
            body: bytes::Bytes::from_static(&[1, 2, 3]),
            byte_order: ByteOrder::Big,
            type_changed: false,
        }));
        let (tx, mut rx) = broadcast::channel::<MonitorUpdate>(8);
        let (tx_raw, mut rx_raw) = broadcast::channel::<crate::pva_gateway::source::RawEvent>(8);

        signal_disconnect_boundary(&state, &latest_raw, &tx, &tx_raw);

        let ev = rx_raw.try_recv().expect("raw boundary event emitted");
        assert!(ev.type_changed, "must be a subscription-boundary marker");
        assert!(ev.body.is_empty(), "boundary marker carries no body");
        assert_eq!(ev.byte_order, ByteOrder::Big);
        let dev = rx.try_recv().expect("decoded boundary event emitted");
        assert!(
            dev.type_changed,
            "decoded fanout must also carry the boundary"
        );
        assert!(
            latest_raw.read().is_none(),
            "latest_raw cleared on disconnect"
        );
        assert!(
            state.read().latest.is_none(),
            "state.latest cleared on disconnect"
        );
    }

    /// Idempotent per outage by construction (replaces the former
    /// `disconnected_alarm_sent` flag): no snapshot ⇒ no emission, and the
    /// first call clears the snapshot so a retry storm cannot re-emit.
    #[test]
    fn disconnect_boundary_is_idempotent_per_outage() {
        use crate::pva_gateway::source::RawEvent;
        use tokio::sync::broadcast;
        let state = RwLock::new(EntryState::default());
        let latest_raw = RwLock::new(None::<crate::pva_gateway::source::RawEvent>);
        let (tx, mut rx) = broadcast::channel::<MonitorUpdate>(8);
        let (tx_raw, mut rx_raw) = broadcast::channel::<crate::pva_gateway::source::RawEvent>(8);

        // No cached snapshot ⇒ nothing to revoke ⇒ no emission on either stream.
        signal_disconnect_boundary(&state, &latest_raw, &tx, &tx_raw);
        assert!(
            rx_raw.try_recv().is_err(),
            "no raw boundary without a prior snapshot"
        );
        assert!(
            rx.try_recv().is_err(),
            "no decoded boundary without a prior snapshot"
        );

        // Arm with a snapshot, fire once — both streams get exactly one.
        *latest_raw.write() = Some(RawEvent {
            body: bytes::Bytes::from_static(&[9]),
            byte_order: ByteOrder::Little,
            type_changed: false,
        });
        signal_disconnect_boundary(&state, &latest_raw, &tx, &tx_raw);
        assert!(
            rx_raw.try_recv().is_ok(),
            "first disconnect emits a raw boundary"
        );
        assert!(
            rx.try_recv().is_ok(),
            "first disconnect emits a decoded boundary"
        );

        // Second call within one outage: snapshot already cleared, no-op on both.
        signal_disconnect_boundary(&state, &latest_raw, &tx, &tx_raw);
        assert!(
            rx_raw.try_recv().is_err(),
            "second call within one outage must not re-emit raw"
        );
        assert!(
            rx.try_recv().is_err(),
            "second call within one outage must not re-emit decoded"
        );
    }

    /// As the single removal owner, `drop_entry` publishes the
    /// removed PV name on the wired channel invalidator so the
    /// native server force-disconnects the downstream channel (the
    /// downstream effect of dropping the cache entry). A drop publishes one
    /// batch carrying the removed name; a drop that removes nothing publishes
    /// nothing.
    #[tokio::test]
    async fn drop_entry_publishes_removed_name_on_invalidator() {
        let client = Arc::new(PvaClient::builder().build());
        let cache = ChannelCache::new(crate::test_reactor(), client, Duration::from_secs(60));
        let inv = ChannelInvalidator::new();
        let mut rx = inv.subscribe();
        cache.set_invalidator(inv.clone());
        cache.insert_test_entry("WM:PV").await;

        assert!(cache.drop_entry("WM:PV").await, "entry was present");
        assert_eq!(
            rx.try_recv().expect("name published on drop").to_vec(),
            vec!["WM:PV".to_string()]
        );

        // A drop that matches no entry removes nothing and publishes nothing.
        assert!(!cache.drop_entry("WM:PV").await, "already gone");
        assert!(
            rx.try_recv().is_err(),
            "a no-op drop must not publish an invalidation"
        );
    }

    /// `flush` publishes every distinct removed name so all downstream
    /// channels bound to dropped entries are force-disconnected, not merely
    /// starved of events — and it does so as ONE batch, not one message per
    /// name. The per-name fan-out (the pre-fix shape) could overflow a bounded
    /// broadcast on a large `:flush`; a single batch over an unbounded
    /// per-connection queue cannot.
    #[tokio::test]
    async fn flush_publishes_all_removed_names_in_one_batch() {
        let client = Arc::new(PvaClient::builder().build());
        let cache = ChannelCache::new(crate::test_reactor(), client, Duration::from_secs(60));
        let inv = ChannelInvalidator::new();
        let mut rx = inv.subscribe();
        cache.set_invalidator(inv.clone());
        cache.insert_test_entry("A:PV").await;
        cache.insert_test_entry("B:PV").await;

        assert_eq!(cache.flush().await, 2, "both channels removed");
        // One flush = one batch carrying both names. HashMap iteration order
        // is unspecified, so sort the batch before comparing.
        let mut got = rx.try_recv().expect("flush published one batch").to_vec();
        got.sort();
        assert_eq!(got, vec!["A:PV".to_string(), "B:PV".to_string()]);
        assert!(
            rx.try_recv().is_err(),
            "a flush publishes exactly one batch, not one message per name"
        );
    }

    /// One PV asked for under several distinct pvRequests is ONE cached
    /// channel with several unique subscriptions — pva2pva keys the top-level
    /// `ChannelCache::entries` by channel name and nests the per-pvRequest
    /// monitors in `ChannelCacheEntry::mon_entries` (`chancache.h:108-125`).
    /// `cacheSize` (`entry_count`), the admission cap, and `cleanerRemoved`
    /// must therefore count CHANNELS, not monitor variants; the report row
    /// surfaces the variant count separately as pva2pva's `<n>` unique
    /// subscription(s) (`p2pApp/server.cpp:218,228`).
    ///
    /// Pre-fix the flat `(pv_name, pv_request)` map made all three of these
    /// count variants, so this same PV reported `cacheSize=3`, ate three
    /// admission slots, and bumped `cleanerRemoved` by three. By boundary:
    /// one channel / three variants → entry_count==1, status row
    /// subscriptions==3, drop removes the whole channel.
    #[tokio::test]
    async fn monitor_variants_count_as_one_channel() {
        let client = Arc::new(PvaClient::builder().build());
        let cache = ChannelCache::new(crate::test_reactor(), client, Duration::from_secs(60));
        // Same PV name, three distinct serialized-pvRequest keys.
        cache.insert_test_variant("VAR:PV", Vec::new()).await;
        cache.insert_test_variant("VAR:PV", vec![1, 2, 3]).await;
        cache.insert_test_variant("VAR:PV", vec![4, 5, 6]).await;
        // …and an unrelated second channel.
        cache.insert_test_entry("OTHER:PV").await;

        // cacheSize counts channels: 2, not 4 monitor variants.
        assert_eq!(
            cache.entry_count().await,
            2,
            "cacheSize must count channels, not pvRequest variants"
        );

        // The status row for the multi-variant channel reports its three
        // unique subscriptions, while the cache lists two channel rows.
        let status = cache.entry_status(64).await;
        assert_eq!(status.total, 2, "report total counts channels");
        let var = status
            .entries
            .iter()
            .find(|e| e.pv_name == "VAR:PV")
            .expect("VAR:PV channel row present");
        assert_eq!(
            var.subscriptions, 3,
            "channel row reports its unique-subscription (variant) count"
        );

        // Dropping the channel takes all three variants with it.
        assert!(cache.drop_entry("VAR:PV").await, "channel was present");
        assert_eq!(
            cache.entry_count().await,
            1,
            "dropping a channel removes all its variants"
        );
    }

    /// A connection-only admission (`has_pv`/`pvconnect` routed through the
    /// cache) is counted in `cacheSize`, survives exactly one idle cleanup
    /// tick of grace before ageing out, and is reachable by `drop`/`flush`
    /// — the accounting half that keeps a connectionless probe's channel
    /// visible to the operator surface instead of an untracked client entry.
    #[tokio::test]
    async fn connection_admission_counts_ages_out_and_drops() {
        let client = Arc::new(PvaClient::builder().build());
        let cache = ChannelCache::new(crate::test_reactor(), client, Duration::from_secs(60));

        cache.insert_test_connection("CONN:PV").await;
        assert_eq!(
            cache.entry_count().await,
            1,
            "connection-only admission counts in cacheSize"
        );
        // It is reported as a connected channel with zero subscriptions.
        let status = cache.entry_status(64).await;
        let row = status
            .entries
            .iter()
            .find(|e| e.pv_name == "CONN:PV")
            .expect("connection admission has a status row");
        assert!(row.connected, "a connected probe reports connected");
        assert_eq!(
            row.subscriptions, 0,
            "connection-only channel has no monitors"
        );

        // One grace tick keeps it; the next idle tick ages it out.
        cache.cleanup_tick_for_test().await;
        assert_eq!(
            cache.entry_count().await,
            1,
            "connection admission survives one grace tick"
        );
        cache.cleanup_tick_for_test().await;
        assert_eq!(
            cache.entry_count().await,
            0,
            "idle connection admission ages out on the next tick"
        );

        // Reachable by drop_entry and flush.
        cache.insert_test_connection("CONN:PV").await;
        assert!(
            cache.drop_entry("CONN:PV").await,
            "connection admission is droppable by name"
        );
        cache.insert_test_connection("A:PV").await;
        cache.insert_test_connection("B:PV").await;
        assert_eq!(
            cache.flush().await,
            2,
            "flush removes connection-only admissions"
        );
        assert_eq!(cache.entry_count().await, 0);
    }

    /// A cache with no invalidator wired (standalone, outside a server)
    /// still flushes / drops correctly and publishes nothing — the
    /// `OnceLock`-guarded publish path is a clean no-op rather than a panic.
    #[tokio::test]
    async fn unwired_cache_drop_and_flush_are_silent() {
        let client = Arc::new(PvaClient::builder().build());
        let cache = ChannelCache::new(crate::test_reactor(), client, Duration::from_secs(60));
        cache.insert_test_entry("WM:PV").await;
        assert!(cache.drop_entry("WM:PV").await);
        cache.insert_test_entry("WM:PV2").await;
        assert_eq!(cache.flush().await, 1);
    }
}
