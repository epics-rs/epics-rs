//! `sync` — trigger-PV-gated event passthrough (epics-base 3.15.7 channel
//! filters).
//!
//! pvxs JSON syntax: `PV.{"sync":{"m":"before"|"after"|"first"|"last"|"unless"|"while"}}`
//! `trigger` is the PV name whose process events drive the gate.
//!
//! Semantics implemented here are the simplest "after" mode: arm the
//! gate when the trigger PV processes; the very next value event from
//! the subscriber consumes the arm and is forwarded, all other value
//! events are dropped. Alarm and property events pass unchanged
//! (446e0d4a rule) and do not consume the arm — those are out-of-band
//! signals every filter must let through.
//!
//! Trigger wiring is via a process-wide [`SyncRegistry`]: filter
//! construction registers an `Arc<AtomicBool>` keyed by trigger PV
//! name, and the trigger PV's process path calls
//! [`SyncRegistry::fire`] (typically from
//! [`crate::server::pv::ProcessVariable::notify_subscribers`]) which
//! arms every gate listening for that PV name. The full upstream
//! syncfilter supports five additional modes (`before`, `first`,
//! `last`, `unless`, `while`) — those land alongside as the trigger
//! plumbing matures.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use parking_lot::Mutex;

use super::{FilteredMonitorEvent, SubscriptionFilter};
use crate::server::recgbl::EventMask;

/// Process-wide map from trigger-PV name to the gates listening for it.
/// Filters register on construction; the trigger PV's process path
/// calls `fire(name)` to arm every gate currently subscribed.
pub struct SyncRegistry {
    gates: Mutex<HashMap<String, Vec<Arc<AtomicBool>>>>,
}

impl Default for SyncRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl SyncRegistry {
    pub fn new() -> Self {
        Self {
            gates: Mutex::new(HashMap::new()),
        }
    }

    /// Subscribe `gate` to fire when `trigger` PV processes.
    pub fn subscribe(&self, trigger: &str, gate: Arc<AtomicBool>) {
        self.gates
            .lock()
            .entry(trigger.to_string())
            .or_default()
            .push(gate);
    }

    /// Arm every gate listening for `trigger`. Stale gates whose
    /// owning filter has been dropped (Arc strong count fell to 1 —
    /// our own table entry is the only ref left) are reaped lazily.
    pub fn fire(&self, trigger: &str) {
        let mut guard = self.gates.lock();
        let Some(list) = guard.get_mut(trigger) else {
            return;
        };
        list.retain(|g| Arc::strong_count(g) > 1);
        for g in list.iter() {
            g.store(true, Ordering::Release);
        }
    }
}

/// The single process-wide [`SyncRegistry`] instance. Filters and
/// record-processing paths share it via [`registry`].
pub fn registry() -> &'static SyncRegistry {
    use std::sync::OnceLock;
    static REGISTRY: OnceLock<SyncRegistry> = OnceLock::new();
    REGISTRY.get_or_init(SyncRegistry::new)
}

/// `sync` filter, "after" mode. Constructed with the trigger PV name;
/// auto-subscribes itself to the process-wide [`SyncRegistry`] so the
/// trigger PV's `fire(name)` call arms this gate.
pub struct SyncFilter {
    /// Shared with [`SyncRegistry`] so the trigger's `fire(name)` call
    /// can flip this from outside.
    armed: Arc<AtomicBool>,
}

impl SyncFilter {
    /// Construct a sync filter for the given trigger PV name. The
    /// gate starts disarmed — events are dropped until the trigger
    /// PV processes for the first time.
    pub fn new(trigger: impl Into<String>) -> Self {
        let armed = Arc::new(AtomicBool::new(false));
        registry().subscribe(&trigger.into(), armed.clone());
        Self { armed }
    }

    /// Manually arm the gate. Useful for unit tests that don't want
    /// to spin up a trigger PV; callers usually rely on the
    /// auto-registration set up in [`Self::new`].
    pub fn arm(&self) {
        self.armed.store(true, Ordering::Release);
    }

    /// `true` iff the next value event will be consumed instead of
    /// dropped. Exposed for tests; not part of the runtime path.
    pub fn is_armed(&self) -> bool {
        self.armed.load(Ordering::Acquire)
    }
}

impl SubscriptionFilter for SyncFilter {
    fn name(&self) -> &'static str {
        "sync"
    }

    fn apply(&self, event: FilteredMonitorEvent) -> Option<FilteredMonitorEvent> {
        // 446e0d4a rule: alarm and property events pass without
        // consuming the gate.
        if !event.mask.contains(EventMask::VALUE) {
            return Some(event);
        }
        // Consume the arm if set; otherwise drop.
        match self
            .armed
            .compare_exchange(true, false, Ordering::AcqRel, Ordering::Acquire)
        {
            Ok(_) => Some(event),
            Err(_) => None,
        }
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
        FilteredMonitorEvent::new(
            MonitorEvent {
                snapshot: Snapshot::new(EpicsValue::Double(v), 0, 0, SystemTime::UNIX_EPOCH),
                origin: 0,
            },
            mask,
        )
    }

    #[test]
    fn disarmed_drops_value_events() {
        let f = SyncFilter::new("UNIT:TEST:TRIGGER:1");
        assert!(!f.is_armed());
        assert!(f.apply(ev(1.0, EventMask::VALUE)).is_none());
        assert!(f.apply(ev(2.0, EventMask::VALUE)).is_none());
    }

    #[test]
    fn arm_passes_next_value_and_disarms() {
        let f = SyncFilter::new("UNIT:TEST:TRIGGER:2");
        f.arm();
        assert!(f.is_armed());
        assert!(f.apply(ev(1.0, EventMask::VALUE)).is_some());
        assert!(!f.is_armed(), "consumed the arm");
        assert!(f.apply(ev(2.0, EventMask::VALUE)).is_none());
    }

    /// Alarm / property events MUST pass without consuming the arm
    /// (446e0d4a rule). Otherwise an alarm-only emission would silence
    /// the next value event the trigger meant to release.
    #[test]
    fn alarm_passes_without_consuming_arm() {
        let f = SyncFilter::new("UNIT:TEST:TRIGGER:3");
        f.arm();
        assert!(f.apply(ev(0.0, EventMask::ALARM)).is_some());
        assert!(f.is_armed(), "alarm did not consume the arm");
        assert!(f.apply(ev(0.0, EventMask::PROPERTY)).is_some());
        assert!(f.is_armed(), "property did not consume the arm");
        // Now a value event consumes it.
        assert!(f.apply(ev(1.0, EventMask::VALUE)).is_some());
        assert!(!f.is_armed());
    }

    /// `SyncRegistry::fire(name)` arms every gate listening for that
    /// PV name. Demonstrates the trigger plumbing end-to-end without
    /// needing a real record-processing chain.
    #[test]
    fn registry_fire_arms_all_matching_gates() {
        let a = SyncFilter::new("UNIT:TEST:TRIGGER:4");
        let b = SyncFilter::new("UNIT:TEST:TRIGGER:4");
        let other = SyncFilter::new("UNIT:TEST:TRIGGER:5");
        assert!(!a.is_armed() && !b.is_armed() && !other.is_armed());

        registry().fire("UNIT:TEST:TRIGGER:4");
        assert!(a.is_armed(), "gate A subscribed to trigger:4 must arm");
        assert!(b.is_armed(), "gate B subscribed to trigger:4 must arm");
        assert!(
            !other.is_armed(),
            "gate listening for a different trigger must NOT arm"
        );
    }

    /// Dropped filters get reaped from the registry on next fire —
    /// no permanent leak in the gate list.
    #[test]
    fn registry_reaps_dropped_filters_on_fire() {
        {
            let _short_lived = SyncFilter::new("UNIT:TEST:TRIGGER:6");
            // Goes out of scope here — strong count on its `armed`
            // Arc falls to 1 (only the registry's clone remains).
        }
        // Fire to trigger reap; subsequent registry state should not
        // include the dropped filter.
        registry().fire("UNIT:TEST:TRIGGER:6");
        let guard = registry().gates.lock();
        let entry = guard.get("UNIT:TEST:TRIGGER:6");
        // After reap the list is empty (entry may still exist as Vec<>).
        assert!(entry.map(|v| v.is_empty()).unwrap_or(true));
    }
}
