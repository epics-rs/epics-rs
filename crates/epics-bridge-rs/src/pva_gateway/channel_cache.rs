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

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::RwLock;
use tokio::sync::{Mutex, Notify, broadcast};

use epics_pva_rs::client::PvaClient;
use epics_pva_rs::client_native::ops_v2::Pauser;
use epics_pva_rs::pvdata::{FieldDesc, PvField};

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

/// Negative-result LRU bound + TTL. After a `lookup` fails (timeout
/// or upstream error), we record the name with a timestamp so the
/// next ~30 s of `has_pv` / `is_writable` probes for the same name
/// short-circuit to "not found" instead of re-spawning an upstream
/// monitor task. Mirrors p2pApp `chancache.h:118` `dropPoke`
/// semantics but bounded so a probe-storm cannot grow it forever.
const NEG_CACHE_MAX: usize = 1024;
const NEG_CACHE_TTL: Duration = Duration::from_secs(30);

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
    tx: broadcast::Sender<PvField>,
    /// F-G12 raw-frame fan-out. Carries the upstream MONITOR DATA
    /// body (`changed | value | overrun`) as a refcounted
    /// `bytes::Bytes` so N downstream subscribers all share the
    /// same allocation. Server-side `subscribe_raw` returns a
    /// receiver from this sender; the monitor task pumps both
    /// `tx` (decoded PvField, for `subscribe()` and snapshot) and
    /// `tx_raw` (raw bytes) per upstream event.
    tx_raw: broadcast::Sender<crate::pva_gateway::source::RawEvent>,
    /// Pulsed on the first successful upstream event. `lookup()` waits
    /// on this so callers see a populated snapshot before returning.
    first_event: Arc<Notify>,
    /// Background upstream monitor task. Aborted on entry drop.
    _monitor_task: AbortOnDrop,
    /// Sticky "recently used" bit, lowered by the cleanup tick.
    drop_poke: parking_lot::Mutex<bool>,
    /// Pause/resume handle on the *current* upstream subscription.
    /// Refreshed by the auto-restart loop on every successful
    /// `pvmonitor_handle` cycle. `None` while the loop is in the gap
    /// between disconnects. Used by [`Self::pauser_snapshot`]
    /// (PG-G9: forward downstream watermark events into upstream
    /// pipeline-pause control msgs). Shared between the spawned
    /// monitor task (writer) and the source's notify_watermark_*
    /// callbacks (reader) via Arc.
    pauser: Arc<parking_lot::Mutex<Option<Pauser>>>,
}

#[derive(Default)]
struct EntryState {
    /// Most recent value seen on the upstream monitor.
    latest: Option<PvField>,
    /// Type descriptor learned from the first INIT response.
    introspection: Option<FieldDesc>,
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
        return MonitorEventOutcome::default();
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
    }
}

impl UpstreamEntry {
    /// Latest cached value; cheap clone of the `PvField` enum.
    pub fn snapshot(&self) -> Option<PvField> {
        self.state.read().latest.clone()
    }

    /// Cached introspection if known.
    pub fn introspection(&self) -> Option<FieldDesc> {
        self.state.read().introspection.clone()
    }

    /// Subscribe to upstream events. The receiver is fresh — pre-existing
    /// values are NOT replayed (broadcast semantics). Callers needing
    /// the current value should also call [`Self::snapshot`].
    pub fn subscribe(&self) -> broadcast::Receiver<PvField> {
        self.poke();
        self.tx.subscribe()
    }

    /// F-G12: raw-frame subscriber. Receives upstream MONITOR DATA
    /// body bytes verbatim. Server uses this to skip its own
    /// `encode_pv_field` step.
    pub fn subscribe_raw(&self) -> broadcast::Receiver<crate::pva_gateway::source::RawEvent> {
        self.poke();
        self.tx_raw.subscribe()
    }

    /// Number of live downstream subscribers (broadcast receivers).
    pub fn subscriber_count(&self) -> usize {
        // Count both fan-out streams: typed PvField subscribers AND
        // raw-frame subscribers. The upstream monitor task should
        // stay alive when either path has consumers.
        self.tx.receiver_count() + self.tx_raw.receiver_count()
    }

    fn poke(&self) {
        *self.drop_poke.lock() = true;
    }

    /// Pause the upstream pipeline if a `Pauser` is currently
    /// installed (the auto-restart loop refreshes this on every
    /// cycle). Returns `None` if upstream isn't connected right now —
    /// caller can't await anything in that case; reaction is best-
    /// effort. The returned future, when awaited, sends the
    /// pipeline-pause control to the upstream server.
    pub fn pauser_snapshot(&self) -> Option<Pauser> {
        self.pauser.lock().clone()
    }
}

/// Drop guard that aborts a tokio task when the entry is dropped.
struct AbortOnDrop(tokio::task::AbortHandle);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// Process-wide cache. Handed to the gateway server source as an
/// `Arc<ChannelCache>`; cheap to clone (only the Arc is bumped).
pub struct ChannelCache {
    client: Arc<PvaClient>,
    /// Map of PV name → entry.
    entries: Arc<Mutex<HashMap<String, Arc<UpstreamEntry>>>>,
    /// Cleanup-tick handle. Aborted on `ChannelCache` drop.
    cleanup_task: parking_lot::Mutex<Option<tokio::task::JoinHandle<()>>>,
    /// Hard cap on `entries.len()` — defends against probe-storm DoS
    /// where a client searches N random names and forces N upstream
    /// monitor tasks. New inserts past this limit return
    /// `GwError::CacheFull` so the downstream sees a clean error.
    max_entries: usize,
    /// Bounded LRU of recently-failed lookups (name + when failure
    /// was recorded). VecDeque + linear scan is fine at NEG_CACHE_MAX
    /// = 1024 entries; we trade a constant-factor cost for not
    /// pulling in an LRU crate. Entries past NEG_CACHE_TTL are
    /// pruned lazily on the next negative-cache hit.
    negative_cache: parking_lot::Mutex<VecDeque<(String, Instant)>>,
}

impl ChannelCache {
    /// Build a cache that will route upstream requests through `client`.
    /// Spawns a periodic cleanup task with the given interval; pass
    /// [`DEFAULT_CLEANUP_INTERVAL`] to match p2pApp's 30 s. The
    /// resulting cache uses [`DEFAULT_MAX_ENTRIES`] for its ceiling;
    /// override via [`Self::with_max_entries`] before publishing the
    /// `Arc` if a larger or smaller cap is needed.
    pub fn new(client: Arc<PvaClient>, cleanup_interval: Duration) -> Arc<Self> {
        Self::with_max_entries(client, cleanup_interval, DEFAULT_MAX_ENTRIES)
    }

    /// Variant of [`Self::new`] with an explicit max-entries cap.
    pub fn with_max_entries(
        client: Arc<PvaClient>,
        cleanup_interval: Duration,
        max_entries: usize,
    ) -> Arc<Self> {
        let cache = Arc::new(Self {
            client,
            entries: Arc::new(Mutex::new(HashMap::new())),
            cleanup_task: parking_lot::Mutex::new(None),
            max_entries,
            negative_cache: parking_lot::Mutex::new(VecDeque::with_capacity(NEG_CACHE_MAX)),
        });
        let weak = Arc::downgrade(&cache);
        let task = tokio::spawn(async move {
            let mut tick = tokio::time::interval(cleanup_interval);
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

    /// True if `name` is in the negative-result LRU and its entry is
    /// still within `NEG_CACHE_TTL`. Lazily prunes expired entries.
    fn is_recently_failed(&self, name: &str) -> bool {
        let now = Instant::now();
        let mut neg = self.negative_cache.lock();
        // Lazy prune (cheap at 1024 entries).
        while let Some((_, t)) = neg.front() {
            if now.duration_since(*t) >= NEG_CACHE_TTL {
                neg.pop_front();
            } else {
                break;
            }
        }
        neg.iter().any(|(n, _)| n == name)
    }

    /// Record `name` as recently-failed. FIFO eviction past
    /// [`NEG_CACHE_MAX`].
    fn record_failure(&self, name: &str) {
        let mut neg = self.negative_cache.lock();
        if neg.iter().any(|(n, _)| n == name) {
            return; // already there
        }
        if neg.len() >= NEG_CACHE_MAX {
            neg.pop_front();
        }
        neg.push_back((name.to_string(), Instant::now()));
    }

    /// Public accessor for the underlying client. Used by the source
    /// to issue one-shot GET / PUT through the same connection pool.
    pub fn client(&self) -> &Arc<PvaClient> {
        &self.client
    }

    /// Cheap, non-spawning probe for "is this PV in the cache right
    /// now?". Returns the cached entry if present (and pokes its
    /// recency bit), or `None` without inserting / spawning.
    ///
    /// Used by `is_writable` and similar advisory paths so a
    /// downstream client probing N random PV names cannot trigger N
    /// upstream search-and-spawn cycles. The full `lookup` path
    /// remains for `has_pv`/`get_value`/`subscribe` etc. that
    /// genuinely need to resolve.
    pub async fn peek(&self, pv_name: &str) -> Option<Arc<UpstreamEntry>> {
        let map = self.entries.lock().await;
        let existing = map.get(pv_name).cloned();
        if let Some(ref e) = existing {
            e.poke();
        }
        existing
    }

    /// Look up or create the entry for `pv_name`. Waits up to
    /// `connect_timeout` for the first upstream event so downstream
    /// callers see a populated `snapshot()` before this returns.
    /// Mirrors `pva2pva ChannelCache::lookup` blocking on `isConnected()`.
    ///
    /// Concurrency: spawn-and-insert happens under the same lock, so
    /// two concurrent lookups for the same PV cannot each spawn an
    /// upstream monitor task. The wait for the first upstream event
    /// happens AFTER the lock is released so the lock is never held
    /// across the network round-trip.
    ///
    /// **Negative-result handling**: if the upstream never delivers a
    /// first event within `connect_timeout`, the freshly-inserted
    /// entry is removed before returning the error. This prevents a
    /// search storm vector where a typo'd PV name would otherwise
    /// pin an upstream-monitor task on every `has_pv` call until the
    /// next 30 s cleanup tick (review §3f).
    ///
    /// **Cancel safety**: cleanup of the freshly-inserted entry uses
    /// a drop guard so an awaiting future being cancelled
    /// (`tokio::select!` losing, deadline-exceeded wrapper, etc.) does
    /// not leave the cache pinned.
    pub async fn lookup(
        &self,
        pv_name: &str,
        connect_timeout: Duration,
    ) -> GwResult<Arc<UpstreamEntry>> {
        // Negative-result short-circuit: if this name failed recently
        // we don't pay for another upstream search. Saves a per-name
        // upstream-monitor task in probe-storm scenarios.
        if self.is_recently_failed(pv_name) {
            return Err(GwError::UpstreamTimeout(pv_name.to_string()));
        }
        let (entry, was_fresh) = {
            let mut map = self.entries.lock().await;
            if let Some(existing) = map.get(pv_name) {
                existing.poke();
                (existing.clone(), false)
            } else {
                if map.len() >= self.max_entries {
                    // PG-G11 spurious-reject mitigation: pre-sweep
                    // entries pinned only by the cache itself
                    // (strong_count==1 means no other Arc holders —
                    // no live UpstreamEntry references via subscribe,
                    // no in-flight CleanupGuard. These are just
                    // waiting for the next cleanup tick to evict.)
                    map.retain(|_, e| Arc::strong_count(e) > 1);
                }
                if map.len() >= self.max_entries {
                    tracing::warn!(
                        pv = %pv_name,
                        len = map.len(),
                        cap = self.max_entries,
                        "pva-gateway: channel cache full, refusing new entry"
                    );
                    return Err(GwError::CacheFull(self.max_entries));
                }
                let fresh = self.spawn_upstream_monitor(pv_name);
                map.insert(pv_name.to_string(), fresh.clone());
                (fresh, true)
            }
        };

        // Drop guard: removes the entry on early-exit (timeout OR
        // cancellation). Disarmed on success.
        struct CleanupGuard<'a> {
            cache: &'a ChannelCache,
            pv_name: &'a str,
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
                // F-G1/F-G6: also record a negative-cache hit so a
                // cancellation race (caller's outer timeout / abort
                // dropping the future before await_first_event
                // returns Err) doesn't leave the next lookup
                // re-spawning the same upstream search immediately.
                self.cache.record_failure(self.pv_name);
                if let Ok(mut map) = self.cache.entries.try_lock() {
                    map.remove(self.pv_name);
                    return;
                }
                // Lock contended — spawn a tiny task that takes the
                // async lock and removes the orphan. Without this,
                // the orphan survives a full cleanup TTL because
                // cleanup_tick treats drop_poke=true (initial state)
                // as "recently used, keep".
                let entries = self.cache.entries.clone();
                let pv_name = self.pv_name.to_string();
                tokio::spawn(async move {
                    entries.lock().await.remove(&pv_name);
                });
            }
        }

        let mut guard = CleanupGuard {
            cache: self,
            pv_name,
            armed: was_fresh,
        };
        match self.await_first_event(entry, connect_timeout).await {
            Ok(e) => {
                guard.disarm();
                Ok(e)
            }
            Err(e) => {
                // Negative-result LRU: record so a probe-storm of N
                // bad names doesn't keep paying the connect_timeout
                // cost. Guard still fires on drop to remove the
                // pinned entry.
                self.record_failure(pv_name);
                Err(e)
            }
        }
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
    fn spawn_upstream_monitor(&self, pv_name: &str) -> Arc<UpstreamEntry> {
        let (tx, _rx0) = broadcast::channel::<PvField>(BROADCAST_CAPACITY);
        let (tx_raw, _rx0_raw) =
            broadcast::channel::<crate::pva_gateway::source::RawEvent>(BROADCAST_CAPACITY);
        let first_event = Arc::new(Notify::new());
        let state = Arc::new(RwLock::new(EntryState::default()));
        let pauser_slot: Arc<parking_lot::Mutex<Option<Pauser>>> =
            Arc::new(parking_lot::Mutex::new(None));

        let pv_name_owned = pv_name.to_string();
        let client = self.client.clone();
        let tx_for_task = tx.clone();
        let tx_raw_for_task = tx_raw.clone();
        let state_for_task = state.clone();
        let first_event_for_task = first_event.clone();
        let pauser_slot_for_task = pauser_slot.clone();

        let join = tokio::spawn(async move {
            let mut backoff = Duration::from_millis(250);
            let max_backoff = Duration::from_secs(30);
            loop {
                let tx_inner = tx_for_task.clone();
                let state_inner = state_for_task.clone();
                let first_event_inner = first_event_for_task.clone();
                let _pv_name_for_cb = pv_name_owned.clone();

                // F-G12 final form: TRUE wire-bytes forwarding via
                // `pvmonitor_raw_frames_handle` — the upstream monitor
                // task never decodes the value. The body bytes flow
                // straight from upstream socket → broadcast →
                // downstream socket. We only decode lazily when
                // `state.latest` is genuinely needed (the cache's
                // first-event signal + future typed `subscribe()`
                // callers, which today are unused for the gateway
                // path).
                //
                // PG-G9 Pauser: the `_handle` variant returns a
                // SubscriptionHandle whose `pauser()` we install in
                // `pauser_slot_for_task`. Downstream watermark events
                // (DownstreamWatermark::High/Low) call into the slot
                // and pause/resume the upstream pipeline on the wire
                // — pvxs `MonitorControlOp::pipeline` parity.
                let tx_raw_inner = tx_raw_for_task.clone();
                let pv_clone = pv_name_owned.clone();
                // BR-R41: tx_inner moves into the callback so decoded
                // events fan out to typed subscribers (subscribe_inner /
                // subscribe_checked fallback path). Pre-fix this sender
                // was dropped here before the closure captured it, so
                // bcast_rx.recv() in subscribe_inner blocked forever
                // after the initial snapshot.
                let handle_result = client
                    .pvmonitor_raw_frames_handle(&pv_name_owned, move |desc, body, order| {
                        let outcome = apply_monitor_event(&state_inner, desc, &body, order);
                        use crate::pva_gateway::source::RawEvent;
                        if outcome.type_changed {
                            tracing::warn!(
                                pv = %pv_clone,
                                "pva-gateway: upstream introspection changed — \
                                 emitting type-change boundary to downstream monitors \
                                 (cache descriptor reset)"
                            );
                            // BR-R42: pvxs treats reconnect/type-change as
                            // a subscription boundary
                            // (pvalink_channel.cpp:342-351 `onTypeChange()`).
                            // Forwarding the new body under the downstream's
                            // original MONITOR INIT descriptor would deliver
                            // bytes the client can't decode — possibly causing
                            // a protocol error or silently corrupted values.
                            // Emit a marker event with no body so the
                            // downstream dispatch path sends MONITOR FINISH
                            // and the client knows to reopen with a fresh
                            // INIT against the new descriptor.
                            let _ = tx_raw_inner.send(RawEvent {
                                body: bytes::Bytes::new(),
                                byte_order: order,
                                type_changed: true,
                            });
                            // Skip the normal body forward — the bytes are
                            // for the NEW descriptor; sending them under
                            // the old INIT descriptor is exactly the
                            // BR-R42 bug.
                            return;
                        }
                        if outcome.was_first {
                            first_event_inner.notify_waiters();
                        }
                        // BR-R41: fan out decoded value to typed subscribers
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
                                let _ = tx_inner.send(val);
                            }
                        }
                        // Fan out raw body — refcount only, no copy.
                        let _ = tx_raw_inner.send(RawEvent {
                            body,
                            byte_order: order,
                            type_changed: false,
                        });
                    })
                    .await;
                // `pvmonitor_raw_frames_handle` returns immediately
                // with a handle whose internal task drives the
                // monitor loop. We install the pauser into the slot
                // for downstream watermark callbacks, then wait for
                // the task to terminate (clean disconnect, channel
                // close, or fatal error).
                let handle = match handle_result {
                    Ok(h) => h,
                    Err(e) => {
                        tracing::warn!(
                            pv = %pv_name_owned,
                            error = %e,
                            backoff_ms = backoff.as_millis() as u64,
                            "pva-gateway: raw upstream monitor failed to start, will retry"
                        );
                        if tx_raw_for_task.receiver_count() == 0 {
                            return;
                        }
                        tokio::time::sleep(backoff).await;
                        backoff = std::cmp::min(backoff * 2, max_backoff);
                        continue;
                    }
                };
                *pauser_slot_for_task.lock() = Some(handle.pauser());
                let raw_result = handle.wait().await;
                *pauser_slot_for_task.lock() = None;
                if let Err(e) = raw_result {
                    tracing::warn!(
                        pv = %pv_name_owned,
                        error = %e,
                        backoff_ms = backoff.as_millis() as u64,
                        "pva-gateway: raw upstream monitor failed, will retry"
                    );
                    if tx_raw_for_task.receiver_count() == 0 {
                        return;
                    }
                    tokio::time::sleep(backoff).await;
                    backoff = std::cmp::min(backoff * 2, max_backoff);
                    continue;
                }
                backoff = Duration::from_millis(250);

                // Both typed (PvField) and raw-frame channels feed
                // downstreams; F-G12 raw-forwarding is default-on so
                // most production subscribers ride tx_raw and tx is
                // empty. Only exit when BOTH have no live receivers,
                // otherwise upstream IOC restart silently kills every
                // raw-path downstream monitor.
                if tx_for_task.receiver_count() == 0 && tx_raw_for_task.receiver_count() == 0 {
                    tracing::debug!(
                        pv = %pv_name_owned,
                        "pva-gateway: monitor exit (no subscribers)"
                    );
                    return;
                }
            }
        });

        Arc::new(UpstreamEntry {
            pv_name: pv_name.to_string(),
            state,
            tx,
            tx_raw,
            first_event,
            _monitor_task: AbortOnDrop(join.abort_handle()),
            drop_poke: parking_lot::Mutex::new(true),
            pauser: pauser_slot,
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
        let res = tokio::time::timeout(connect_timeout, &mut notified).await;
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
        map.retain(|_, entry| {
            let mut poke = entry.drop_poke.lock();
            if *poke {
                *poke = false;
                return true; // recently used — keep
            }
            // Idle for one full tick. Drop unless someone is still
            // listening to the broadcast channel.
            entry.subscriber_count() > 0
        });
    }

    /// Snapshot of cached PV names — used by `ChannelSource::list_pvs`.
    pub async fn names(&self) -> Vec<String> {
        self.entries.lock().await.keys().cloned().collect()
    }

    /// Diagnostic: total entries in the cache.
    pub async fn entry_count(&self) -> usize {
        self.entries.lock().await.len()
    }

    /// B6: operator-driven cache flush. Drops every cached
    /// `UpstreamEntry` and clears the negative-result LRU, then
    /// returns the number of entries that were removed.
    ///
    /// Each removed entry's `AbortOnDrop` aborts its upstream monitor
    /// task once the last `Arc<UpstreamEntry>` is released; any live
    /// downstream subscriber holding a `broadcast::Receiver` simply
    /// stops receiving events (the next downstream search re-opens a
    /// fresh upstream monitor). This mirrors `pva2pva`'s manual
    /// channel-cache drop.
    pub async fn flush(&self) -> usize {
        let mut map = self.entries.lock().await;
        let removed = map.len();
        map.clear();
        self.negative_cache.lock().clear();
        removed
    }

    /// B6: drop a single cache entry by exact PV name. Returns `true`
    /// if an entry was present and removed, `false` if the name was
    /// not cached. Also evicts the name from the negative-result LRU
    /// so a subsequent search re-resolves immediately.
    pub async fn drop_entry(&self, pv_name: &str) -> bool {
        let removed = self.entries.lock().await.remove(pv_name).is_some();
        self.negative_cache.lock().retain(|(n, _)| n != pv_name);
        removed
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
            state.read().latest,
            Some(PvField::Scalar(ScalarValue::Double(1.0)))
        );

        // Second event: value = 2.0. Pre-fix this was DROPPED — the
        // snapshot would still read 1.0.
        let body2 = encode_body(&desc, &PvField::Scalar(ScalarValue::Double(2.0)), &[0]);
        apply_monitor_event(&state, &desc, &body2, ByteOrder::Little);
        assert_eq!(
            state.read().latest,
            Some(PvField::Scalar(ScalarValue::Double(2.0))),
            "snapshot must reflect the 2nd monitor event, not freeze at the 1st"
        );

        // Third event: value = 3.0.
        let body3 = encode_body(&desc, &PvField::Scalar(ScalarValue::Double(3.0)), &[0]);
        apply_monitor_event(&state, &desc, &body3, ByteOrder::Little);
        assert_eq!(
            state.read().latest,
            Some(PvField::Scalar(ScalarValue::Double(3.0))),
            "snapshot must track the live value across many events"
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
        assert_eq!(state.read().latest, Some(full(10, 20)));

        // Delta event: only `a` changed → bit 1. `b` is unmarked and
        // arrives zero-filled; the merge must restore b=20.
        let body2 = encode_body(&desc, &full(99, 0), &[1]);
        apply_monitor_event(&state, &desc, &body2, ByteOrder::Little);
        assert_eq!(
            state.read().latest,
            Some(full(99, 20)),
            "delta merge must keep unmarked field `b` at its prior value"
        );
    }

    /// BR-R42: `apply_monitor_event` flags a descriptor change so
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
            "introspection change must be flagged for the BR-R42 marker path"
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
        let (tx, rx0) = broadcast::channel::<PvField>(4);
        drop(rx0);
        let (tx_raw, rx0_raw) = broadcast::channel::<crate::pva_gateway::source::RawEvent>(4);
        drop(rx0_raw);
        let task = tokio::spawn(async {
            tokio::time::sleep(Duration::from_secs(60)).await;
        });
        let entry = UpstreamEntry {
            pv_name: "X".into(),
            state: Arc::new(RwLock::new(EntryState::default())),
            tx,
            tx_raw,
            first_event: Arc::new(Notify::new()),
            _monitor_task: AbortOnDrop(task.abort_handle()),
            drop_poke: parking_lot::Mutex::new(false),
            pauser: Arc::new(parking_lot::Mutex::new(None)),
        };
        assert_eq!(entry.subscriber_count(), 0);
        let _r1 = entry.subscribe();
        let _r2 = entry.subscribe();
        assert_eq!(entry.subscriber_count(), 2);
        assert!(*entry.drop_poke.lock(), "subscribe must poke");
    }
}
