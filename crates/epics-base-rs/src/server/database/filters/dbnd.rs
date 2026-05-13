//! `dbnd` — value deadband filter (epics-base 3.15.7 channel filters).
//!
//! Suppresses value-change `MonitorEvent`s when the new value sits
//! inside a `±threshold` band around the last value the subscriber
//! actually saw. Alarm and property events always pass through —
//! epics-base 446e0d4a fixed the C `dbnd` so it never gates
//! `DBE_ALARM` / `DBE_PROPERTY` (a stale property silenced by a
//! deadband would desync clients).
//!
//! pvxs CA filter wire syntax: `PV.{"dbnd":{"d":0.5}}` (abs) or
//! `PV.{"dbnd":{"r":0.5}}` (relative to last value). The parser
//! that lifts this off the channel name is not yet implemented;
//! this commit ships the filter itself, ready to be plumbed in by
//! the JSON parser commit.

use parking_lot::Mutex;

use super::{FilteredMonitorEvent, SubscriptionFilter};
use crate::server::recgbl::EventMask;
use crate::types::EpicsValue;

/// Deadband mode — absolute or relative-to-last-value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeadbandMode {
    /// `|new - last| >= threshold` passes. The default for the
    /// `d` JSON key.
    Absolute,
    /// `|new - last| >= threshold * |last|` passes. The `r` JSON
    /// key. When `last == 0.0` we fall back to absolute comparison
    /// against `threshold` directly so the first non-zero sample
    /// after start-up can never be silenced by a zero-relative gate.
    Relative,
}

/// `dbnd` filter.
///
/// State: `last_sent` records the value that was most recently
/// forwarded to the subscriber. The first event always passes (no
/// prior value to compare against) so the subscriber observes the
/// initial snapshot before the filter kicks in.
pub struct DeadbandFilter {
    threshold: f64,
    mode: DeadbandMode,
    last_sent: Mutex<Option<f64>>,
}

impl DeadbandFilter {
    pub fn new(threshold: f64, mode: DeadbandMode) -> Self {
        Self {
            threshold: threshold.abs(),
            mode,
            last_sent: Mutex::new(None),
        }
    }

    /// Convenience: absolute-deadband filter with the given threshold.
    pub fn absolute(threshold: f64) -> Self {
        Self::new(threshold, DeadbandMode::Absolute)
    }

    /// Convenience: relative-deadband filter (fraction).
    pub fn relative(fraction: f64) -> Self {
        Self::new(fraction, DeadbandMode::Relative)
    }
}

impl SubscriptionFilter for DeadbandFilter {
    fn name(&self) -> &'static str {
        "dbnd"
    }

    fn apply(&self, event: FilteredMonitorEvent) -> Option<FilteredMonitorEvent> {
        // 446e0d4a: never gate ALARM / PROPERTY events. Pass them
        // through (and DO NOT update `last_sent` — the value didn't
        // change, only the alarm or metadata did).
        if !event.mask.contains(EventMask::VALUE) {
            return Some(event);
        }

        // We only know how to compare numeric values. String /
        // boolean / array values pass unconditionally — a future
        // `arr` filter handles arrays, and strings have no obvious
        // deadband semantic.
        let Some(cur) = event.event.snapshot.value.to_f64() else {
            return Some(event);
        };

        let mut last = self.last_sent.lock();
        let pass = match *last {
            // First event ever: always pass and seed the state.
            None => true,
            Some(prev) => {
                let delta = (cur - prev).abs();
                match self.mode {
                    DeadbandMode::Absolute => delta >= self.threshold,
                    DeadbandMode::Relative => {
                        let scale = prev.abs();
                        if scale == 0.0 {
                            // Fall back to absolute when prev is 0 — see module note.
                            delta >= self.threshold
                        } else {
                            delta >= self.threshold * scale
                        }
                    }
                }
            }
        };

        if pass {
            *last = Some(cur);
            Some(event)
        } else {
            None
        }
    }
}

/// Hint helper for the future JSON parser — distinguishes `d` vs `r`.
#[allow(dead_code)] // wired in when the PV-name JSON parser lands
pub(crate) fn parse_mode(key: &str) -> Option<DeadbandMode> {
    match key {
        "d" => Some(DeadbandMode::Absolute),
        "r" => Some(DeadbandMode::Relative),
        _ => None,
    }
}

// `EpicsValue` referenced from doc tests — keep the import live.
#[allow(dead_code)]
fn _doctype(v: &EpicsValue) -> Option<f64> {
    v.to_f64()
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

    /// First event always passes — there is no `last_sent` baseline yet.
    #[test]
    fn first_value_passes() {
        let f = DeadbandFilter::absolute(0.5);
        assert!(f.apply(ev(10.0, EventMask::VALUE)).is_some());
    }

    /// Absolute deadband: drops sub-threshold deltas, passes deltas
    /// at or above threshold (inclusive — matches C dbnd behaviour).
    #[test]
    fn absolute_deadband_gates_subthreshold() {
        let f = DeadbandFilter::absolute(1.0);
        assert!(f.apply(ev(10.0, EventMask::VALUE)).is_some());
        assert!(
            f.apply(ev(10.4, EventMask::VALUE)).is_none(),
            "0.4 < 1.0 must be silenced"
        );
        assert!(
            f.apply(ev(11.0, EventMask::VALUE)).is_some(),
            "1.0 == threshold passes (>= semantics)"
        );
        assert!(
            f.apply(ev(11.5, EventMask::VALUE)).is_none(),
            "11.5 - 11.0 = 0.5 < 1.0"
        );
        assert!(f.apply(ev(12.5, EventMask::VALUE)).is_some());
    }

    /// Negative-direction deltas honour `|delta|`.
    #[test]
    fn absolute_deadband_is_symmetric() {
        let f = DeadbandFilter::absolute(1.0);
        assert!(f.apply(ev(100.0, EventMask::VALUE)).is_some());
        assert!(f.apply(ev(98.5, EventMask::VALUE)).is_some());
        assert!(f.apply(ev(98.0, EventMask::VALUE)).is_none());
        assert!(f.apply(ev(97.0, EventMask::VALUE)).is_some());
    }

    /// Relative deadband: threshold scales with `|last|`. 1% of 100
    /// is 1.0 — same shape as absolute_deadband_gates_subthreshold
    /// but the scale comes from the previous value.
    #[test]
    fn relative_deadband_scales_with_last_value() {
        let f = DeadbandFilter::relative(0.01); // 1%
        assert!(f.apply(ev(100.0, EventMask::VALUE)).is_some());
        assert!(f.apply(ev(100.5, EventMask::VALUE)).is_none()); // 0.5% < 1%
        assert!(f.apply(ev(101.0, EventMask::VALUE)).is_some()); // 1% passes
        assert!(f.apply(ev(101.5, EventMask::VALUE)).is_none()); // 0.495% of 101
    }

    /// When `last == 0` the relative filter falls back to absolute
    /// comparison against the configured fraction (so the next
    /// non-zero sample is never trapped by `threshold * 0`).
    #[test]
    fn relative_deadband_zero_baseline_uses_absolute() {
        let f = DeadbandFilter::relative(0.5);
        assert!(f.apply(ev(0.0, EventMask::VALUE)).is_some());
        assert!(
            f.apply(ev(0.4, EventMask::VALUE)).is_none(),
            "0.4 < 0.5 (abs fallback) — silenced"
        );
        assert!(
            f.apply(ev(0.6, EventMask::VALUE)).is_some(),
            "0.6 >= 0.5 (abs fallback) — passes"
        );
    }

    /// 446e0d4a — `dbnd` MUST NOT gate `DBE_ALARM`. The event passes
    /// even if the value didn't change, and `last_sent` stays put
    /// (it tracks values, not alarms).
    #[test]
    fn alarm_events_pass_without_updating_state() {
        let f = DeadbandFilter::absolute(10.0);
        // Seed value-state with 100.
        assert!(f.apply(ev(100.0, EventMask::VALUE)).is_some());
        // 101 alone would be silenced by VALUE deadband (delta 1 < 10).
        // But an ALARM-tagged event must still pass.
        assert!(f.apply(ev(101.0, EventMask::ALARM)).is_some());
        // The alarm pass-through did NOT update last_sent, so a fresh
        // 101 with VALUE mask is still silenced (compared against 100).
        assert!(f.apply(ev(101.0, EventMask::VALUE)).is_none());
    }

    /// Same rule for `DBE_PROPERTY`.
    #[test]
    fn property_events_pass_without_updating_state() {
        let f = DeadbandFilter::absolute(10.0);
        assert!(f.apply(ev(50.0, EventMask::VALUE)).is_some());
        assert!(f.apply(ev(50.5, EventMask::PROPERTY)).is_some());
        // last_sent untouched — 50.5 still inside the deadband around 50.
        assert!(f.apply(ev(50.5, EventMask::VALUE)).is_none());
    }

    /// Non-numeric values (e.g. strings) pass unconditionally — there
    /// is no numeric deadband to apply.
    #[test]
    fn non_numeric_passes() {
        let f = DeadbandFilter::absolute(1.0);
        let snap = Snapshot::new(
            EpicsValue::String("hello".into()),
            0,
            0,
            SystemTime::UNIX_EPOCH,
        );
        let event = FilteredMonitorEvent::new(
            MonitorEvent {
                snapshot: snap,
                origin: 0,
            },
            EventMask::VALUE,
        );
        assert!(f.apply(event).is_some());
    }

    #[test]
    fn parse_mode_recognises_d_and_r() {
        assert_eq!(parse_mode("d"), Some(DeadbandMode::Absolute));
        assert_eq!(parse_mode("r"), Some(DeadbandMode::Relative));
        assert_eq!(parse_mode("nope"), None);
    }
}
