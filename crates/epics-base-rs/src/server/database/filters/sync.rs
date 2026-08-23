//! `sync` — dbState-gated event passthrough (epics-base 3.15.7 channel
//! filters).
//!
//! pvxs / dbCore JSON syntax:
//!
//! ```text
//! PV.{"sync":{"m":"before"|"first"|"last"|"after"|"while"|"unless","s":"STATE"}}
//! ```
//!
//! `s` (or the mode-tagged shorthand `{"sync":{"after":"STATE"}}`) names
//! a global [`DbState`] — a boolean held by the process-wide
//! [`db_state_registry`] and toggled via [`DbState::set`]. Records
//! that act as "triggers" call `set` from their process path; every
//! filter listening on that state name sees the transition on its
//! next `apply()`.
//!
//! Six modes, all per epics-base `db/std/filters/sync.c`:
//!
//! | mode     | semantics                                                 |
//! |----------|------------------------------------------------------------|
//! | `before` | cache every event; emit cached on state transition `0→1` |
//! | `first`  | emit first event seen after state transition `0→1`; drop rest until state cycles |
//! | `last`   | cache every event; emit cached on state transition `1→0` |
//! | `after`  | emit first event seen after state transition `1→0`; drop rest until state cycles |
//! | `while`  | pass events while state is `1`; drop while `0`            |
//! | `unless` | pass events while state is `0`; drop while `1`            |
//!
//! Per epics-base `sync.c::filter` only `DBE_PROPERTY` (and read-
//! context — Rust has no analog) bypasses the state machine
//! unconditionally. `DBE_ALARM` events run through the configured
//! mode just like value events — the 446e0d4a "always pass alarm"
//! rule is dbnd-specific.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use parking_lot::Mutex;

use super::{FilteredMonitorEvent, SubscriptionFilter};
use crate::server::recgbl::EventMask;

/// Global named boolean state — the analogue of epics-base `dbState`.
/// Cloning the `Arc` returned by [`DbStateRegistry::get_or_create`]
/// is cheap; the underlying `AtomicBool` is shared across all
/// subscribers and the trigger record's set/clear call sites.
#[derive(Debug, Default)]
pub struct DbState {
    inner: AtomicBool,
}

impl DbState {
    pub fn set(&self, value: bool) {
        self.inner.store(value, Ordering::Release);
    }
    pub fn get(&self) -> bool {
        self.inner.load(Ordering::Acquire)
    }
}

/// Process-wide named-state registry. Filters lazily acquire the
/// `Arc<DbState>` for their configured state name; trigger paths use
/// the same accessor to publish state changes.
pub struct DbStateRegistry {
    states: Mutex<HashMap<String, Arc<DbState>>>,
}

impl Default for DbStateRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl DbStateRegistry {
    pub fn new() -> Self {
        Self {
            states: Mutex::new(HashMap::new()),
        }
    }

    /// Look up or create the named state. Existing entry's `Arc` is
    /// returned unchanged — the underlying value is preserved.
    pub fn get_or_create(&self, name: &str) -> Arc<DbState> {
        let mut guard = self.states.lock();
        if let Some(s) = guard.get(name) {
            return s.clone();
        }
        let s = Arc::new(DbState::default());
        guard.insert(name.to_string(), s.clone());
        s
    }

    /// Look up the named state without creating it — C `dbStateFind`
    /// (`dbState.c`), which returns `NULL` for an unknown name. The `Db State`
    /// device support uses this to emit C's one-time "creating new db state"
    /// notice (`devBiDbState`/`devBoDbState` `add_record`) only on the create
    /// path, where [`get_or_create`](Self::get_or_create) alone cannot tell a
    /// fresh state from a pre-existing one.
    pub fn find(&self, name: &str) -> Option<Arc<DbState>> {
        self.states.lock().get(name).cloned()
    }

    /// Convenience: set a named state's value. Creates the state if
    /// it didn't exist (with the requested value).
    pub fn set(&self, name: &str, value: bool) {
        self.get_or_create(name).set(value);
    }

    /// Convenience: read a named state's value. Returns `false` when
    /// the state has never been touched (creates it implicitly).
    pub fn get(&self, name: &str) -> bool {
        self.get_or_create(name).get()
    }
}

/// The single process-wide [`DbStateRegistry`] instance. Filters and
/// trigger record-processing paths share it via [`db_state_registry`].
pub fn db_state_registry() -> &'static DbStateRegistry {
    use std::sync::OnceLock;
    static REGISTRY: OnceLock<DbStateRegistry> = OnceLock::new();
    REGISTRY.get_or_init(DbStateRegistry::new)
}

/// Sync filter mode — six variants matching epics-base `sync.c`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyncMode {
    Before,
    First,
    Last,
    After,
    While,
    Unless,
}

/// `sync` filter. Holds:
///
/// * `mode` — which of the six gating policies to apply.
/// * `state` — the shared `Arc<DbState>` flipped by the trigger.
/// * `last_state` — most recent state value the filter observed,
///   used to detect transitions inside `apply`.
/// * `cached_event` — for `before`/`last` modes that emit a delayed
///   event on state transition. Replaced on every incoming value
///   event so the most recent observation wins.
pub struct SyncFilter {
    mode: SyncMode,
    state: Arc<DbState>,
    last_state: AtomicBool,
    cached_event: Mutex<Option<FilteredMonitorEvent>>,
}

impl SyncFilter {
    /// Takes the state itself, not its name: `sync.c`'s `parse_ok`
    /// resolves the name with `dbStateFind` and returns -1 when it is
    /// unknown (`sync.c:87-93`), so a filter over an undeclared state is
    /// not a thing C can build. Handing the resolved `Arc<DbState>` in
    /// keeps that true by construction here — the filter has no way to
    /// create one. `parser::build_sync` is the resolver.
    pub fn new(mode: SyncMode, state: Arc<DbState>) -> Self {
        Self {
            mode,
            last_state: AtomicBool::new(state.get()),
            state,
            cached_event: Mutex::new(None),
        }
    }

    /// `true` when the underlying named state is currently set. Test
    /// helper.
    pub fn state_value(&self) -> bool {
        self.state.get()
    }
}

impl SubscriptionFilter for SyncFilter {
    fn name(&self) -> &'static str {
        "sync"
    }

    fn apply(&self, event: FilteredMonitorEvent) -> Option<FilteredMonitorEvent> {
        // C `sync.c:98`: `if (pfl->ctx == dbfl_context_read ||
        // (pfl->mask & DBE_PROPERTY)) return pfl;` — a single-read
        // emission and a `DBE_PROPERTY` event both bypass the state
        // machine unchanged. DBE_ALARM runs through the configured
        // mode like a value event. The 446e0d4a rule applies to
        // dbnd, not sync.
        if event.read_context || event.event.mask.intersects(EventMask::PROPERTY) {
            return Some(event);
        }
        let actstate = self.state.get();
        let laststate = self.last_state.load(Ordering::Acquire);
        let transition_up = actstate && !laststate; // 0 → 1
        let transition_down = !actstate && laststate; // 1 → 0

        let pass = match self.mode {
            SyncMode::Before => {
                let mut cache = self.cached_event.lock();
                let out = if transition_up { cache.take() } else { None };
                // Always cache the incoming event for the *next*
                // transition. Old cache contents (if no transition
                // fired) are replaced by the new value.
                *cache = Some(event.clone());
                out
            }
            SyncMode::First => {
                if transition_up {
                    Some(event.clone())
                } else {
                    None
                }
            }
            SyncMode::Last => {
                let mut cache = self.cached_event.lock();
                let out = if transition_down { cache.take() } else { None };
                *cache = Some(event.clone());
                out
            }
            SyncMode::After => {
                if transition_down {
                    Some(event.clone())
                } else {
                    None
                }
            }
            SyncMode::While => {
                if actstate {
                    Some(event.clone())
                } else {
                    None
                }
            }
            SyncMode::Unless => {
                if !actstate {
                    Some(event.clone())
                } else {
                    None
                }
            }
        };
        // Update transition tracker. `While`/`Unless` don't shift the
        // tracker — the C source uses a `no_shift` label for those —
        // but our `last_state` only matters to the four transition-
        // sensitive modes, so storing for `While`/`Unless` is a
        // harmless no-op.
        self.last_state.store(actstate, Ordering::Release);
        pass
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::pv::MonitorEvent;
    use crate::server::snapshot::Snapshot;
    use crate::types::EpicsValue;
    use std::time::SystemTime;

    fn ev(v: f64, mask: EventMask) -> FilteredMonitorEvent {
        FilteredMonitorEvent::new(MonitorEvent {
            snapshot: std::sync::Arc::new(Snapshot::new(
                EpicsValue::Double(v),
                0,
                0,
                SystemTime::UNIX_EPOCH,
            )),
            origin: 0,
            mask,
        })
    }

    fn val(e: &FilteredMonitorEvent) -> f64 {
        match e.event.snapshot.value {
            EpicsValue::Double(v) => v,
            _ => panic!("test ev() always builds Double"),
        }
    }

    // ── While / Unless: stateful pass / drop ───────────────────────

    #[test]
    fn while_passes_when_state_set() {
        let f = SyncFilter::new(
            SyncMode::While,
            db_state_registry().get_or_create("UNIT:T:WHILE:1"),
        );
        // state starts false → drop
        assert!(f.apply(ev(1.0, EventMask::VALUE)).is_none());
        db_state_registry().set("UNIT:T:WHILE:1", true);
        assert_eq!(val(&f.apply(ev(2.0, EventMask::VALUE)).unwrap()), 2.0);
        assert_eq!(val(&f.apply(ev(3.0, EventMask::VALUE)).unwrap()), 3.0);
        db_state_registry().set("UNIT:T:WHILE:1", false);
        assert!(f.apply(ev(4.0, EventMask::VALUE)).is_none());
    }

    #[test]
    fn unless_passes_when_state_clear() {
        let f = SyncFilter::new(
            SyncMode::Unless,
            db_state_registry().get_or_create("UNIT:T:UNLESS:1"),
        );
        // state starts false → pass
        assert_eq!(val(&f.apply(ev(1.0, EventMask::VALUE)).unwrap()), 1.0);
        db_state_registry().set("UNIT:T:UNLESS:1", true);
        assert!(f.apply(ev(2.0, EventMask::VALUE)).is_none());
        db_state_registry().set("UNIT:T:UNLESS:1", false);
        assert_eq!(val(&f.apply(ev(3.0, EventMask::VALUE)).unwrap()), 3.0);
    }

    // ── First / After: pass-once-on-transition ─────────────────────

    #[test]
    fn first_passes_first_event_after_0_to_1_transition() {
        let f = SyncFilter::new(
            SyncMode::First,
            db_state_registry().get_or_create("UNIT:T:FIRST:1"),
        );
        // No transition yet → drop
        assert!(f.apply(ev(0.0, EventMask::VALUE)).is_none());
        // Flip state up. The NEXT apply observes the transition.
        db_state_registry().set("UNIT:T:FIRST:1", true);
        assert_eq!(val(&f.apply(ev(1.0, EventMask::VALUE)).unwrap()), 1.0);
        // Subsequent events with state still 1 → drop (no transition).
        assert!(f.apply(ev(2.0, EventMask::VALUE)).is_none());
        // Bounce state to cycle the transition.
        db_state_registry().set("UNIT:T:FIRST:1", false);
        // The apply that observes 1→0 doesn't emit for `first`; it
        // just updates last_state. But to make the test deterministic
        // we feed an alarm event (no state shift since alarm bypasses
        // the tracker per 446e0d4a — but actually NOT for `first`/
        // `after`: those only fire on transitions of *value* events,
        // and our impl updates last_state on every value event).
        // Send a value event with state=false: drops + updates
        // last_state to false.
        assert!(f.apply(ev(3.0, EventMask::VALUE)).is_none());
        // Now flip back up: next value passes again.
        db_state_registry().set("UNIT:T:FIRST:1", true);
        assert_eq!(val(&f.apply(ev(4.0, EventMask::VALUE)).unwrap()), 4.0);
    }

    #[test]
    fn after_passes_first_event_after_1_to_0_transition() {
        let f = SyncFilter::new(
            SyncMode::After,
            db_state_registry().get_or_create("UNIT:T:AFTER:1"),
        );
        // Start with state=1 so the first downward transition is
        // observable.
        db_state_registry().set("UNIT:T:AFTER:1", true);
        // Prime last_state by applying one value event.
        assert!(f.apply(ev(0.0, EventMask::VALUE)).is_none());
        // 1→0
        db_state_registry().set("UNIT:T:AFTER:1", false);
        assert_eq!(val(&f.apply(ev(1.0, EventMask::VALUE)).unwrap()), 1.0);
        assert!(f.apply(ev(2.0, EventMask::VALUE)).is_none());
    }

    // ── Before / Last: cache-then-emit-on-transition ───────────────

    #[test]
    fn before_emits_cached_pre_transition_event() {
        let f = SyncFilter::new(
            SyncMode::Before,
            db_state_registry().get_or_create("UNIT:T:BEFORE:1"),
        );
        // Pre-transition events get cached, dropped from the stream.
        assert!(f.apply(ev(10.0, EventMask::VALUE)).is_none());
        assert!(f.apply(ev(20.0, EventMask::VALUE)).is_none());
        // Flip 0→1 → the NEXT apply emits whatever was cached (the
        // latest pre-transition value), then caches the incoming.
        db_state_registry().set("UNIT:T:BEFORE:1", true);
        let emitted = f.apply(ev(30.0, EventMask::VALUE)).unwrap();
        assert_eq!(
            val(&emitted),
            20.0,
            "emits the most recent cached pre-transition value"
        );
        // No further transitions → drops, but keeps caching.
        assert!(f.apply(ev(40.0, EventMask::VALUE)).is_none());
    }

    #[test]
    fn last_emits_cached_on_downward_transition() {
        let f = SyncFilter::new(
            SyncMode::Last,
            db_state_registry().get_or_create("UNIT:T:LAST:1"),
        );
        db_state_registry().set("UNIT:T:LAST:1", true);
        // While state=1: events get cached, dropped.
        assert!(f.apply(ev(10.0, EventMask::VALUE)).is_none());
        assert!(f.apply(ev(20.0, EventMask::VALUE)).is_none());
        // 1→0: emits the cached event (latest while active).
        db_state_registry().set("UNIT:T:LAST:1", false);
        let emitted = f.apply(ev(30.0, EventMask::VALUE)).unwrap();
        assert_eq!(val(&emitted), 20.0);
    }

    // ── Bypass: only DBE_PROPERTY short-circuits (sync.c parity) ────

    #[test]
    fn property_event_passes_unconditionally_for_every_mode() {
        for mode in [
            SyncMode::Before,
            SyncMode::First,
            SyncMode::Last,
            SyncMode::After,
            SyncMode::While,
            SyncMode::Unless,
        ] {
            let f = SyncFilter::new(mode, db_state_registry().get_or_create("UNIT:T:PROP"));
            assert!(
                f.apply(ev(0.0, EventMask::PROPERTY)).is_some(),
                "{mode:?} must pass property"
            );
        }
    }

    /// `while` mode with state=false drops DBE_ALARM along with the
    /// usual value events — C `sync.c::syncModeWhile` doesn't
    /// special-case the alarm bit.
    #[test]
    fn alarm_event_runs_through_state_machine_in_while_mode() {
        let state_name = "UNIT:T:ALARM_GATED";
        let f = SyncFilter::new(
            SyncMode::While,
            db_state_registry().get_or_create(state_name),
        );
        db_state_registry().set(state_name, false);
        assert!(
            f.apply(ev(0.0, EventMask::ALARM)).is_none(),
            "While + state=false must drop ALARM, matching sync.c"
        );
        db_state_registry().set(state_name, true);
        assert!(
            f.apply(ev(0.0, EventMask::ALARM)).is_some(),
            "While + state=true passes ALARM"
        );
    }

    // ── Registry semantics ─────────────────────────────────────────

    #[test]
    fn registry_get_or_create_returns_shared_state() {
        let a = db_state_registry().get_or_create("UNIT:T:SHARED");
        let b = db_state_registry().get_or_create("UNIT:T:SHARED");
        a.set(true);
        assert!(b.get(), "both Arcs view the same AtomicBool");
    }
}
