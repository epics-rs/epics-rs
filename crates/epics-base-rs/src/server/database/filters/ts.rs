//! `ts` — timestamp pass-through / rate-limit filter
//! (epics-base 3.15.7 channel filters).
//!
//! The C `ts` filter forwards the event but overwrites the snapshot
//! timestamp with the *current* wall clock at the moment the filter
//! runs (i.e. when the event reaches the subscriber, not when the
//! record was processed). Useful for clients that want client-side
//! arrival times stamped server-side rather than the record's
//! intrinsic timestamp.
//!
//! pvxs syntax: `PV.{"ts":{}}` (no options).
//!
//! Alarm and property events also have their timestamp rewritten —
//! the filter doesn't gate emission, only timestamp.

use super::{FilteredMonitorEvent, SubscriptionFilter};

pub struct TimestampFilter;

impl TimestampFilter {
    pub fn new() -> Self {
        Self
    }
}

impl Default for TimestampFilter {
    fn default() -> Self {
        Self::new()
    }
}

impl SubscriptionFilter for TimestampFilter {
    fn name(&self) -> &'static str {
        "ts"
    }

    fn apply(&self, mut event: FilteredMonitorEvent) -> Option<FilteredMonitorEvent> {
        event.event.snapshot.timestamp = crate::runtime::time::now_wall();
        Some(event)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::pv::MonitorEvent;
    use crate::server::recgbl::EventMask;
    use crate::server::snapshot::Snapshot;
    use crate::types::EpicsValue;
    use std::time::{Duration, SystemTime};

    fn make_event(t: SystemTime) -> FilteredMonitorEvent {
        FilteredMonitorEvent::new(
            MonitorEvent {
                snapshot: Snapshot::new(EpicsValue::Double(1.0), 0, 0, t),
                origin: 0,
            },
            EventMask::VALUE,
        )
    }

    /// The filter rewrites the snapshot timestamp to "now". The
    /// pre-filter sentinel (UNIX_EPOCH) must NOT survive — any
    /// post-EPOCH timestamp is acceptable.
    #[test]
    fn rewrites_snapshot_timestamp_to_now() {
        let f = TimestampFilter::new();
        let before = SystemTime::now();
        let out = f.apply(make_event(SystemTime::UNIX_EPOCH)).unwrap();
        let stamped = out.event.snapshot.timestamp;
        assert!(
            stamped >= before - Duration::from_millis(1),
            "stamp must reflect current wall clock"
        );
    }

    /// Filter is purely transformational — never drops.
    #[test]
    fn never_drops_an_event() {
        let f = TimestampFilter::new();
        assert!(f.apply(make_event(SystemTime::UNIX_EPOCH)).is_some());
    }

    /// Alarm-only emissions also get re-stamped.
    #[test]
    fn restamps_alarm_events_too() {
        let f = TimestampFilter::new();
        let mut ev = make_event(SystemTime::UNIX_EPOCH);
        ev.mask = EventMask::ALARM;
        let out = f.apply(ev).unwrap();
        assert!(out.event.snapshot.timestamp > SystemTime::UNIX_EPOCH);
    }
}
