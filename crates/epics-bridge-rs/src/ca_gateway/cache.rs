//! PV cache for the CA gateway.
//!
//! Corresponds to C++ `gateServer` PV cache (`pv_list`, `pv_con_list`,
//! `vc_list`) plus the per-PV state machine in `gatePvData`.
//!
//! ## State machine
//!
//! ```text
//!   ┌──────┐  upstream search  ┌────────────┐  connect callback  ┌──────────┐
//!   │ Dead ├──────────────────►│ Connecting ├───────────────────►│ Inactive │
//!   └──────┘                   └─────┬──────┘                    └────┬─────┘
//!      ▲                             │                                │
//!      │                             │ timeout                first subscriber
//!      │                             ▼                                │
//!      │                       ┌──────────┐                           ▼
//!      └───────────────────────┤   Dead   │                      ┌────────┐
//!                              └──────────┘                      │ Active │
//!                                                                └────┬───┘
//!      ┌────────────┐                                                 │
//!      │ Disconnect │◄──── upstream disconnect (Inactive)             │
//!      └─────┬──────┘                                                 │
//!            │                                                        │
//!            │ timeout                                                │
//!            ▼                                                        │
//!      ┌──────────┐                                                   │
//!      │   Dead   │                last subscriber leaves             │
//!      └──────────┘◄──────────────────────────────────────────────────┘

// RTEMS-EXEC-MODEL-ALLOW(4): checked - these run and pass in the exec-backend
// suite.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use epics_base_rs::server::snapshot::Snapshot;
use tokio::sync::RwLock;

/// State of a cached PV in the gateway.
///
/// Corresponds to C++ `gatePvData` states:
/// `gatePvDead`, `gatePvConnect`, `gatePvInactive`, `gatePvActive`,
/// `gatePvDisconnect`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PvState {
    /// No upstream connection, no clients.
    Dead,
    /// Upstream connect in progress.
    Connecting,
    /// Upstream connected, no active downstream subscribers.
    Inactive,
    /// Upstream connected, ≥1 downstream subscriber.
    Active,
    /// Upstream connection lost, cleanup pending.
    Disconnect,
}

impl PvState {
    /// Whether the gateway considers this PV "exists" for downstream search.
    pub fn is_existent(self) -> bool {
        matches!(self, Self::Inactive | Self::Active)
    }
}

/// One PV in the gateway cache.
///
/// Tracks the upstream connection state, the most recent value snapshot
/// (for serving cached reads), the list of downstream subscriber IDs
/// (for fan-out), and timing information for cleanup heuristics.
#[derive(Debug)]
pub struct GwPvEntry {
    /// Upstream PV name (after alias resolution).
    pub name: String,
    /// Current state in the lifecycle FSM.
    pub state: PvState,
    /// Most recent value + metadata received from upstream.
    /// `None` until the first event arrives after upstream connection.
    pub cached: Option<Snapshot>,
    /// Subscription IDs of downstream clients monitoring this PV.
    /// Used as a reference count: when empty, the PV transitions
    /// from `Active` to `Inactive`.
    pub subscribers: Vec<u32>,
    /// Synthetic IDs of downstream clients that have an *open monitor*
    /// (`CA_PROTO_EVENT_ADD`) on this PV — a strict subset of
    /// [`Self::subscribers`] (a plain `caget` opens a channel but no
    /// monitor). Drives the no-cache lazy upstream monitor: the upstream
    /// subscription exists only while this is non-empty. Empty (and
    /// unused) in [`CacheMode::Cached`](super::server::CacheMode::Cached),
    /// where the upstream monitor is always present. Mirrors C ca-gateway
    /// `vc->needPosting()` gating `pv->monitor()` under `-no_cache`
    /// (`gatePv.cc:1737-1753`).
    pub monitor_interest: Vec<u32>,
    /// Synthetic IDs of downstream clients with an open `DBE_PROPERTY`
    /// monitor on this PV — a subset of [`Self::monitor_interest`] (a
    /// downstream monitor may select value-only, property-only, or both).
    /// Drives the no-cache lazy upstream *property* monitor independently
    /// of the value monitor: the upstream property subscription exists
    /// only while this is non-empty. Empty (and unused) in
    /// [`CacheMode::Cached`](super::server::CacheMode::Cached), where the
    /// property monitor is always present. Mirrors C ca-gateway gating
    /// `pv->propMonitor()` on `vc->needPosting() && client_mask ==
    /// DBE_PROPERTY` under `-no_cache` (`gatePv.cc:1749-1752`).
    pub prop_interest: Vec<u32>,
    /// When the current state was entered. Used by cleanup to evict
    /// PVs that have been Inactive/Dead/Disconnect for too long.
    pub state_since: Instant,
    /// Total events received from upstream (for stats).
    pub event_count: u64,
    /// cumulative time spent in any "upstream alive" state
    /// (Inactive or Active). Updated by `set_state` whenever the
    /// previous state was alive and we transition out.
    pub total_alive: Duration,
    /// Cumulative time spent in any "upstream not alive" state
    /// (Connecting / Dead / Disconnect). Updated symmetrically.
    pub total_dead: Duration,
}

impl GwPvEntry {
    /// Create a new entry in the `Connecting` state.
    pub fn new_connecting(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            state: PvState::Connecting,
            cached: None,
            subscribers: Vec::new(),
            monitor_interest: Vec::new(),
            prop_interest: Vec::new(),
            state_since: Instant::now(),
            event_count: 0,
            total_alive: Duration::ZERO,
            total_dead: Duration::ZERO,
        }
    }

    /// Transition to a new state and reset the state timestamp.
    /// also accumulate the elapsed time into `total_alive`
    /// or `total_dead` based on the previous state, so operators
    /// can read per-PV uptime histograms via gateway stats.
    pub fn set_state(&mut self, new: PvState) {
        if self.state != new {
            let elapsed = self.state_since.elapsed();
            if self.state.is_existent() {
                self.total_alive = self.total_alive.saturating_add(elapsed);
            } else {
                self.total_dead = self.total_dead.saturating_add(elapsed);
            }
            self.state = new;
            self.state_since = Instant::now();
        }
    }

    /// Add a downstream subscriber. If this is the first subscriber and
    /// the PV is Inactive, transition to Active.
    pub fn add_subscriber(&mut self, sid: u32) {
        if !self.subscribers.contains(&sid) {
            self.subscribers.push(sid);
        }
        if self.state == PvState::Inactive && !self.subscribers.is_empty() {
            self.set_state(PvState::Active);
        }
    }

    /// Remove a downstream subscriber. If this was the last subscriber
    /// and the PV is Active, transition to Inactive.
    pub fn remove_subscriber(&mut self, sid: u32) {
        self.subscribers.retain(|s| *s != sid);
        if self.state == PvState::Active && self.subscribers.is_empty() {
            self.set_state(PvState::Inactive);
        }
    }

    /// How many downstream subscribers are currently attached.
    pub fn subscriber_count(&self) -> usize {
        self.subscribers.len()
    }

    /// Record a downstream monitor (`EVENT_ADD`) opening on this PV.
    /// Returns `true` when this is the *first* monitor — the no-cache
    /// owner uses that edge to create the upstream subscription. No-op
    /// (returns `false`) if `sid` is already counted, so a duplicate or
    /// replayed event cannot double-create the upstream monitor.
    pub fn add_monitor_interest(&mut self, sid: u32) -> bool {
        if self.monitor_interest.contains(&sid) {
            return false;
        }
        let was_empty = self.monitor_interest.is_empty();
        self.monitor_interest.push(sid);
        was_empty
    }

    /// Record a downstream monitor (`EVENT_CANCEL` / channel teardown)
    /// closing on this PV. Returns `true` when this was the *last*
    /// monitor — the no-cache owner uses that edge to drop the upstream
    /// subscription. No-op (returns `false`) if `sid` was not counted.
    pub fn remove_monitor_interest(&mut self, sid: u32) -> bool {
        let before = self.monitor_interest.len();
        self.monitor_interest.retain(|s| *s != sid);
        before != self.monitor_interest.len() && self.monitor_interest.is_empty()
    }

    /// How many downstream monitors are currently open on this PV.
    pub fn monitor_interest_count(&self) -> usize {
        self.monitor_interest.len()
    }

    /// Record a downstream `DBE_PROPERTY` monitor (`EVENT_ADD` with the
    /// property select bit) opening on this PV. Returns `true` when this is
    /// the *first* property monitor — the no-cache owner uses that edge to
    /// create the upstream property monitor. No-op (returns `false`) if
    /// `sid` is already counted, so a duplicate/replayed event cannot
    /// double-create the upstream property subscription.
    pub fn add_property_interest(&mut self, sid: u32) -> bool {
        if self.prop_interest.contains(&sid) {
            return false;
        }
        let was_empty = self.prop_interest.is_empty();
        self.prop_interest.push(sid);
        was_empty
    }

    /// Record a downstream `DBE_PROPERTY` monitor closing on this PV.
    /// Returns `true` when this was the *last* property monitor — the
    /// no-cache owner uses that edge to drop the upstream property monitor.
    /// No-op (returns `false`) if `sid` was not counted (e.g. a value-only
    /// monitor closing), so an unconditional call on every subscription
    /// close is safe.
    pub fn remove_property_interest(&mut self, sid: u32) -> bool {
        let before = self.prop_interest.len();
        self.prop_interest.retain(|s| *s != sid);
        before != self.prop_interest.len() && self.prop_interest.is_empty()
    }

    /// How many downstream `DBE_PROPERTY` monitors are open on this PV.
    pub fn property_interest_count(&self) -> usize {
        self.prop_interest.len()
    }

    /// Update cached snapshot from a new upstream event.
    pub fn update(&mut self, snap: Snapshot) {
        self.cached = Some(snap);
        self.event_count += 1;
    }

    /// Time elapsed in the current state.
    pub fn time_in_state(&self) -> Duration {
        self.state_since.elapsed()
    }
}

/// Timeout configuration for cache cleanup.
///
/// Defaults match C++ ca-gateway:
/// - `connect_timeout`: 1s — drop Connecting PVs that don't reach Inactive
/// - `inactive_timeout`: 2h — drop Inactive PVs with no subscribers
/// - `dead_timeout`: 2min — drop Dead PVs after this delay
/// - `disconnect_timeout`: 2h — drop Disconnect PVs after this delay
#[derive(Debug, Clone, Copy)]
pub struct CacheTimeouts {
    pub connect_timeout: Duration,
    pub inactive_timeout: Duration,
    pub dead_timeout: Duration,
    pub disconnect_timeout: Duration,
}

impl Default for CacheTimeouts {
    fn default() -> Self {
        Self {
            connect_timeout: Duration::from_secs(1),
            inactive_timeout: Duration::from_secs(60 * 60 * 2),
            dead_timeout: Duration::from_secs(60 * 2),
            disconnect_timeout: Duration::from_secs(60 * 60 * 2),
        }
    }
}

/// Gateway PV cache.
///
/// Maps upstream PV name → cache entry. Each entry is wrapped in
/// `Arc<RwLock>` so multiple downstream client tasks and the upstream
/// event handler can share access.
///
/// Corresponds to C++ `gateServer::pv_list` (HashMap of `gatePvData`).
#[derive(Debug, Default)]
pub struct PvCache {
    entries: HashMap<String, Arc<RwLock<GwPvEntry>>>,
}

impl PvCache {
    pub fn new() -> Self {
        Self::default()
    }

    /// Number of entries currently in the cache.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Look up an entry by upstream name.
    pub fn get(&self, name: &str) -> Option<Arc<RwLock<GwPvEntry>>> {
        self.entries.get(name).cloned()
    }

    /// Insert a new entry, replacing any existing one with the same name.
    /// Returns the inserted Arc.
    pub fn insert(&mut self, entry: GwPvEntry) -> Arc<RwLock<GwPvEntry>> {
        let name = entry.name.clone();
        let arc = Arc::new(RwLock::new(entry));
        self.entries.insert(name, arc.clone());
        arc
    }

    /// Get an existing entry or create a new one in the `Connecting` state.
    pub fn get_or_create(&mut self, name: &str) -> Arc<RwLock<GwPvEntry>> {
        if let Some(arc) = self.entries.get(name) {
            return arc.clone();
        }
        self.insert(GwPvEntry::new_connecting(name.to_string()))
    }

    /// Remove an entry by name.
    pub fn remove(&mut self, name: &str) -> Option<Arc<RwLock<GwPvEntry>>> {
        self.entries.remove(name)
    }

    /// All entry names (for stats / introspection).
    pub fn names(&self) -> Vec<String> {
        self.entries.keys().cloned().collect()
    }

    /// Count entries by state.
    ///
    /// snapshots the per-entry Arcs first so the outer cache
    /// `&self` borrow (and therefore any caller-held outer
    /// `RwLock<PvCache>` read guard) is released before we begin
    /// awaiting per-entry RwLocks. Without the snapshot, every
    /// `entry.read().await` yield kept the outer borrow alive,
    /// blocking the cache resolver's writes for the duration of a
    /// large stats refresh.
    pub async fn count_by_state(&self, state: PvState) -> usize {
        let entries: Vec<Arc<RwLock<GwPvEntry>>> = self.entries.values().cloned().collect();
        let mut count = 0;
        for entry in entries {
            if entry.read().await.state == state {
                count += 1;
            }
        }
        count
    }

    /// Like [`Self::count_by_state`] but returns counts for ALL
    /// states in a single pass — saves N×Vec_clone iterations when
    /// the caller wants a full breakdown (the typical Stats refresh
    /// case). Order: (Connecting, Active, Inactive, Dead, Disconnect).
    /// Returns `(connecting, active, inactive, dead, disconnect, vc)`.
    ///
    /// `vc` is the virtual-channel count: cache entries that currently
    /// have ≥1 attached downstream subscriber, regardless of upstream
    /// state. C ca-gateway's `total_vc` (`gateVc.cc:406,472`) is bumped
    /// per `gateVcData` create/destroy — i.e. per PV served to a
    /// downstream client — and is distinct from `total_pv` (cache size).
    /// A subscriber can be attached while the upstream is `Connecting` or
    /// `Disconnect`, so this is NOT the same as the `Active` count.
    pub async fn count_states(&self) -> (usize, usize, usize, usize, usize, usize) {
        let entries: Vec<Arc<RwLock<GwPvEntry>>> = self.entries.values().cloned().collect();
        let mut connecting = 0;
        let mut active = 0;
        let mut inactive = 0;
        let mut dead = 0;
        let mut disconnect = 0;
        let mut vc = 0;
        for entry in entries {
            let guard = entry.read().await;
            if guard.subscriber_count() > 0 {
                vc += 1;
            }
            match guard.state {
                PvState::Connecting => connecting += 1,
                PvState::Active => active += 1,
                PvState::Inactive => inactive += 1,
                PvState::Dead => dead += 1,
                PvState::Disconnect => disconnect += 1,
            }
        }
        (connecting, active, inactive, dead, disconnect, vc)
    }

    /// Sweep expired entries based on timeouts.
    /// Returns the names of removed entries.
    ///
    /// Mirrors `gateServer::connectCleanup` + `inactiveDeadCleanup`. The
    /// FSM is two-stage for the connect-failure path: a `Connecting`
    /// entry that times out is *demoted* to `Dead` first (so a fresh
    /// search from the same client can reuse the upstream subscription
    /// once the IOC reappears) and only evicted after `dead_timeout`
    /// further elapses. Same for `Disconnect` → kept as-is and only
    /// evicted after `disconnect_timeout`.
    pub async fn cleanup(&mut self, timeouts: &CacheTimeouts) -> Vec<String> {
        let mut to_remove = Vec::new();
        let mut to_demote: Vec<String> = Vec::new();

        for (name, entry) in &self.entries {
            let entry_guard = entry.read().await;
            let elapsed = entry_guard.time_in_state();
            match entry_guard.state {
                PvState::Connecting => {
                    if elapsed > timeouts.connect_timeout {
                        to_demote.push(name.clone());
                    }
                }
                PvState::Inactive => {
                    if elapsed > timeouts.inactive_timeout {
                        to_remove.push(name.clone());
                    }
                }
                PvState::Dead => {
                    if elapsed > timeouts.dead_timeout {
                        to_remove.push(name.clone());
                    }
                }
                PvState::Disconnect => {
                    if elapsed > timeouts.disconnect_timeout {
                        to_remove.push(name.clone());
                    }
                }
                PvState::Active => { /* Active PVs are never evicted */ }
            }
        }

        // Demote connect-timeouts to Dead (resets state_since so the
        // dead_timeout window starts now).
        for name in &to_demote {
            if let Some(arc) = self.entries.get(name) {
                arc.write().await.set_state(PvState::Dead);
            }
        }

        for name in &to_remove {
            self.entries.remove(name);
        }
        to_remove
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use epics_base_rs::types::EpicsValue;
    use std::time::SystemTime;

    fn dummy_snapshot(v: f64) -> Snapshot {
        Snapshot::new(EpicsValue::Double(v), 0, 0, SystemTime::now())
    }

    #[test]
    fn pv_state_is_existent() {
        assert!(PvState::Inactive.is_existent());
        assert!(PvState::Active.is_existent());
        assert!(!PvState::Dead.is_existent());
        assert!(!PvState::Connecting.is_existent());
        assert!(!PvState::Disconnect.is_existent());
    }

    #[test]
    fn entry_subscriber_lifecycle() {
        let mut e = GwPvEntry::new_connecting("TEMP");
        assert_eq!(e.state, PvState::Connecting);
        assert_eq!(e.subscriber_count(), 0);

        // Simulate upstream connect → Inactive
        e.set_state(PvState::Inactive);
        assert_eq!(e.state, PvState::Inactive);

        // First subscriber → Active
        e.add_subscriber(1);
        assert_eq!(e.state, PvState::Active);
        assert_eq!(e.subscriber_count(), 1);

        // Second subscriber stays Active
        e.add_subscriber(2);
        assert_eq!(e.state, PvState::Active);
        assert_eq!(e.subscriber_count(), 2);

        // Duplicate add is a no-op
        e.add_subscriber(2);
        assert_eq!(e.subscriber_count(), 2);

        // Remove first subscriber stays Active
        e.remove_subscriber(1);
        assert_eq!(e.state, PvState::Active);
        assert_eq!(e.subscriber_count(), 1);

        // Remove last subscriber → Inactive
        e.remove_subscriber(2);
        assert_eq!(e.state, PvState::Inactive);
        assert_eq!(e.subscriber_count(), 0);
    }

    /// `add_monitor_interest` returns `true` only on the empty→first edge;
    /// `remove_monitor_interest` returns `true` only on the last→empty edge.
    /// These two booleans are what the no-cache owner uses to create and
    /// drop the upstream subscription exactly once.
    #[test]
    fn monitor_interest_edge_transitions() {
        let mut e = GwPvEntry::new_connecting("TEMP");
        assert_eq!(e.monitor_interest_count(), 0);

        // empty → first: became-first edge fires
        assert!(
            e.add_monitor_interest(1),
            "first monitor is the create edge"
        );
        assert_eq!(e.monitor_interest_count(), 1);

        // first → second: no edge
        assert!(
            !e.add_monitor_interest(2),
            "second monitor must not re-fire the create edge"
        );
        assert_eq!(e.monitor_interest_count(), 2);

        // duplicate add: no-op, no edge
        assert!(
            !e.add_monitor_interest(2),
            "duplicate sid cannot double-create the upstream monitor"
        );
        assert_eq!(e.monitor_interest_count(), 2);

        // second → first: not the last, no drop edge
        assert!(
            !e.remove_monitor_interest(1),
            "removing one of two monitors must not fire the drop edge"
        );
        assert_eq!(e.monitor_interest_count(), 1);

        // remove uncounted sid: no-op, no edge
        assert!(
            !e.remove_monitor_interest(999),
            "removing an uncounted sid is a no-op"
        );
        assert_eq!(e.monitor_interest_count(), 1);

        // first → empty: last→empty edge fires
        assert!(
            e.remove_monitor_interest(2),
            "removing the last monitor is the drop edge"
        );
        assert_eq!(e.monitor_interest_count(), 0);

        // remove from empty: no-op, no edge
        assert!(
            !e.remove_monitor_interest(2),
            "removing from an empty set is a no-op"
        );
    }

    /// `add_property_interest` returns `true` only on the empty→first edge;
    /// `remove_property_interest` returns `true` only on the last→empty
    /// edge. These two booleans are what the no-cache owner uses to create
    /// and drop the upstream DBE_PROPERTY monitor exactly once. Boundary
    /// values are enumerated, not narrated.
    #[test]
    fn property_interest_edge_transitions() {
        let mut e = GwPvEntry::new_connecting("TEMP");
        assert_eq!(e.property_interest_count(), 0);

        // empty → first: became-first edge fires (create the prop monitor)
        assert!(
            e.add_property_interest(1),
            "first DBE_PROPERTY monitor is the create edge"
        );
        assert_eq!(e.property_interest_count(), 1);

        // first → second: no edge
        assert!(
            !e.add_property_interest(2),
            "second property monitor must not re-fire the create edge"
        );
        assert_eq!(e.property_interest_count(), 2);

        // duplicate add: no-op, no edge
        assert!(
            !e.add_property_interest(2),
            "duplicate sid cannot double-create the upstream property monitor"
        );
        assert_eq!(e.property_interest_count(), 2);

        // second → first: not the last, no drop edge
        assert!(
            !e.remove_property_interest(1),
            "removing one of two property monitors must not fire the drop edge"
        );
        assert_eq!(e.property_interest_count(), 1);

        // remove uncounted sid: no-op, no edge
        assert!(
            !e.remove_property_interest(999),
            "removing an uncounted sid is a no-op"
        );
        assert_eq!(e.property_interest_count(), 1);

        // first → empty: last→empty edge fires (drop the prop monitor)
        assert!(
            e.remove_property_interest(2),
            "removing the last property monitor is the drop edge"
        );
        assert_eq!(e.property_interest_count(), 0);

        // remove from empty: no-op, no edge
        assert!(
            !e.remove_property_interest(2),
            "removing from an empty set is a no-op"
        );
    }

    /// Value interest and property interest are independent refcounts: a
    /// value-only subscription (no DBE_PROPERTY bit) never adds to
    /// `prop_interest`, so `remove_property_interest` on its sid at close is
    /// a no-op edge — this is what lets `SubscriptionClosed`/`Disconnected`
    /// withdraw property interest unconditionally without spuriously
    /// dropping the upstream property monitor. Conversely a property
    /// subscription adds to BOTH (a DBE_PROPERTY EVENT_ADD also satisfies C
    /// `needPosting()`, gatePv.cc:1737/1749).
    #[test]
    fn value_and_property_interest_are_independent() {
        let mut e = GwPvEntry::new_connecting("TEMP");

        // A value-only subscription: value interest only.
        assert!(e.add_monitor_interest(10));
        assert_eq!(e.monitor_interest_count(), 1);
        assert_eq!(
            e.property_interest_count(),
            0,
            "a value-only subscription must not touch property interest"
        );

        // Withdrawing property interest for that value-only sid is a no-op
        // edge — never fires the prop-monitor drop.
        assert!(
            !e.remove_property_interest(10),
            "withdrawing property interest for a value-only sid must not fire the drop edge"
        );

        // A property subscription adds to both interests. Its value
        // interest is the second value sid (10 already present), so it is
        // NOT the value-monitor create edge; its property interest IS the
        // prop-monitor create edge (first property sid).
        assert!(
            !e.add_monitor_interest(20),
            "second value interest must not re-fire the value-monitor create edge"
        );
        assert!(
            e.add_property_interest(20),
            "first property subscription is the prop-monitor create edge"
        );
        assert_eq!(e.monitor_interest_count(), 2);
        assert_eq!(e.property_interest_count(), 1);

        // Closing the value-only sid drops neither to empty.
        assert!(!e.remove_monitor_interest(10));
        assert_eq!(e.monitor_interest_count(), 1);

        // Closing the property sid: value interest hits empty (drop value
        // monitor) AND property interest hits empty (drop prop monitor).
        assert!(
            e.remove_monitor_interest(20),
            "removing the last value interest is the value-monitor drop edge"
        );
        assert!(
            e.remove_property_interest(20),
            "removing the last property interest is the prop-monitor drop edge"
        );
        assert_eq!(e.monitor_interest_count(), 0);
        assert_eq!(e.property_interest_count(), 0);
    }

    #[test]
    fn entry_update_increments_event_count() {
        let mut e = GwPvEntry::new_connecting("TEMP");
        assert_eq!(e.event_count, 0);
        assert!(e.cached.is_none());

        e.update(dummy_snapshot(1.0));
        assert_eq!(e.event_count, 1);
        assert!(e.cached.is_some());

        e.update(dummy_snapshot(2.0));
        assert_eq!(e.event_count, 2);
    }

    #[tokio::test]
    async fn cache_get_or_create() {
        let mut cache = PvCache::new();
        assert!(cache.is_empty());

        let arc1 = cache.get_or_create("TEMP");
        assert_eq!(cache.len(), 1);
        assert_eq!(arc1.read().await.state, PvState::Connecting);

        // Repeated call returns same Arc
        let arc2 = cache.get_or_create("TEMP");
        assert!(Arc::ptr_eq(&arc1, &arc2));
        assert_eq!(cache.len(), 1);

        // Different name → new entry
        cache.get_or_create("PRESSURE");
        assert_eq!(cache.len(), 2);
    }

    #[tokio::test]
    async fn cache_count_by_state() {
        let mut cache = PvCache::new();
        let a = cache.insert(GwPvEntry::new_connecting("A"));
        let b = cache.insert(GwPvEntry::new_connecting("B"));
        let _c = cache.insert(GwPvEntry::new_connecting("C"));

        a.write().await.set_state(PvState::Active);
        b.write().await.set_state(PvState::Inactive);

        assert_eq!(cache.count_by_state(PvState::Connecting).await, 1);
        assert_eq!(cache.count_by_state(PvState::Inactive).await, 1);
        assert_eq!(cache.count_by_state(PvState::Active).await, 1);
        assert_eq!(cache.count_by_state(PvState::Dead).await, 0);
    }

    #[tokio::test]
    async fn cache_cleanup_removes_expired() {
        let mut cache = PvCache::new();
        let dead = cache.insert(GwPvEntry::new_connecting("DEAD"));
        let active = cache.insert(GwPvEntry::new_connecting("ALIVE"));

        // Put the dead one in Dead state. `state_since` stays "now"; expiry is
        // driven by a zero `dead_timeout` below rather than back-dating the
        // Instant (`Instant - Duration` panics on Windows, where Instant is
        // QPC-since-boot, when uptime is shorter than the span).
        {
            let mut e = dead.write().await;
            e.state = PvState::Dead;
        }
        {
            let mut e = active.write().await;
            e.state = PvState::Active;
        }

        let timeouts = CacheTimeouts {
            dead_timeout: Duration::ZERO,
            ..CacheTimeouts::default()
        };
        let removed = cache.cleanup(&timeouts).await;

        assert_eq!(removed, vec!["DEAD".to_string()]);
        assert!(cache.get("DEAD").is_none());
        assert!(cache.get("ALIVE").is_some());
    }

    #[tokio::test]
    async fn cache_cleanup_demotes_connecting_to_dead() {
        let mut cache = PvCache::new();
        let stuck = cache.insert(GwPvEntry::new_connecting("STUCK"));

        // Drive timing via injected timeouts rather than back-dating
        // `state_since` (`Instant - Duration` panics on Windows when the
        // machine's uptime is shorter than the span). A zero `connect_timeout`
        // demotes the Connecting entry on the first sweep; the default
        // `dead_timeout` keeps it alive that same pass.
        let demote = CacheTimeouts {
            connect_timeout: Duration::ZERO,
            ..CacheTimeouts::default()
        };
        let removed = cache.cleanup(&demote).await;

        // First sweep: still in cache, but now in Dead state.
        assert!(removed.is_empty());
        assert!(cache.get("STUCK").is_some());
        assert_eq!(stuck.read().await.state, PvState::Dead);

        // Second sweep with a zero `dead_timeout`: evicts the now-Dead entry
        // (demotion reset `state_since` to "now", so a zero window suffices).
        let evict = CacheTimeouts {
            dead_timeout: Duration::ZERO,
            ..CacheTimeouts::default()
        };
        let removed = cache.cleanup(&evict).await;
        assert_eq!(removed, vec!["STUCK".to_string()]);
        assert!(cache.get("STUCK").is_none());
    }
}
