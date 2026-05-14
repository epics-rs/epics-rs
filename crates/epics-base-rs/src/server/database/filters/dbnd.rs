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
    /// `|new - last| > threshold` passes. The default for the
    /// `d` JSON key. Comparison is strict — matches C
    /// `recGblCheckDeadband` (`if (delta > deadband)`).
    Absolute,
    /// `|new - last| > threshold * |last|` passes. The `r` JSON
    /// key. `threshold` is the internal fraction (e.g. `0.01` for
    /// 1%); the JSON wire form `r=50` (C-style percent) is divided
    /// by 100 in the parser. When `last == 0.0` the band is `0` so
    /// any non-zero delta passes — matching C `hyst = val * cval/100`
    /// which is zero when `val == 0`.
    Relative,
}

/// `dbnd` filter.
///
/// State: `last_sent` records the value that was most recently
/// forwarded to the subscriber. Initialised to `NaN` so the first
/// finite event produces `delta = INF` (per C `recGblCheckDeadband`
/// NaN-↔-finite rule) and always passes.
pub struct DeadbandFilter {
    threshold: f64,
    mode: DeadbandMode,
    last_sent: Mutex<f64>,
}

impl DeadbandFilter {
    pub fn new(threshold: f64, mode: DeadbandMode) -> Self {
        Self {
            threshold: threshold.abs(),
            mode,
            last_sent: Mutex::new(f64::NAN),
        }
    }

    /// Convenience: absolute-deadband filter with the given threshold.
    pub fn absolute(threshold: f64) -> Self {
        Self::new(threshold, DeadbandMode::Absolute)
    }

    /// Convenience: relative-deadband filter (fraction — 0.01 = 1%).
    pub fn relative(fraction: f64) -> Self {
        Self::new(fraction, DeadbandMode::Relative)
    }
}

/// Mirror C `recGblCheckDeadband`'s delta rule so NaN/Inf transitions
/// always trip the deadband.
fn c_delta(prev: f64, cur: f64) -> f64 {
    if prev.is_finite() && cur.is_finite() {
        (prev - cur).abs()
    } else if prev.is_nan() != cur.is_nan()
        || prev.is_infinite() != cur.is_infinite()
        || (cur.is_infinite() && cur != prev)
    {
        // Mismatched finiteness, or +inf-vs-(-inf): treat as unbounded
        // delta so the deadband always trips.
        f64::INFINITY
    } else {
        0.0
    }
}

impl SubscriptionFilter for DeadbandFilter {
    fn name(&self) -> &'static str {
        "dbnd"
    }

    fn apply(&self, event: FilteredMonitorEvent) -> Option<FilteredMonitorEvent> {
        // Non-numeric values (strings, raw byte arrays, etc.) have no
        // deadband semantic — pass unconditionally and don't touch
        // `last_sent`.
        let Some(cur) = event.event.snapshot.value.to_f64() else {
            return Some(event);
        };

        let mut last = self.last_sent.lock();
        let prev = *last;
        let delta = c_delta(prev, cur);
        let band = match self.mode {
            DeadbandMode::Absolute => self.threshold,
            DeadbandMode::Relative => {
                // C: `hyst = val * cval/100.` is only refreshed after
                // a successful send. We model the same end-state by
                // computing `threshold * |prev|` per call. When `prev`
                // is the initial NaN seed (no successful send yet),
                // fall back to raw `threshold` — C's first call sees
                // `hyst = cval` from `parse_ok` until the first
                // refresh.
                if prev.is_finite() {
                    self.threshold * prev.abs()
                } else {
                    self.threshold
                }
            }
        };
        // C `recGblCheckDeadband`: `if (delta > deadband)` — strict.
        let supra = delta > band;
        if supra {
            // C updates `*poldval = newval` whenever `delta > deadband`,
            // regardless of which mask bits are set. The same write must
            // happen here even when the event is delivered solely because
            // of an ALARM / PROPERTY bit (446e0d4a) so a subsequent
            // VALUE-only emission is compared against the right baseline.
            *last = cur;
        }

        // 446e0d4a: ALARM / PROPERTY / LOG bits guarantee delivery
        // independent of the deadband. VALUE-only emissions are gated
        // by `supra`.
        let non_value = EventMask::from_bits(event.mask.bits() & !EventMask::VALUE.bits());
        if supra || !non_value.is_empty() {
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

    /// Absolute deadband: strict `>` (matches C
    /// `recGblCheckDeadband`). delta == threshold is DROPPED.
    #[test]
    fn absolute_deadband_gates_subthreshold() {
        let f = DeadbandFilter::absolute(1.0);
        assert!(f.apply(ev(10.0, EventMask::VALUE)).is_some());
        assert!(
            f.apply(ev(10.4, EventMask::VALUE)).is_none(),
            "0.4 < 1.0 must be silenced"
        );
        assert!(
            f.apply(ev(11.0, EventMask::VALUE)).is_none(),
            "delta == threshold is dropped under strict > semantics (C dbndTest line 235/241)"
        );
        assert!(
            f.apply(ev(11.5, EventMask::VALUE)).is_some(),
            "1.5 > 1.0 — passes; last_sent advances to 11.5"
        );
        assert!(f.apply(ev(12.5, EventMask::VALUE)).is_none());
    }

    /// Negative-direction deltas honour `|delta|`.
    #[test]
    fn absolute_deadband_is_symmetric() {
        let f = DeadbandFilter::absolute(1.0);
        assert!(f.apply(ev(100.0, EventMask::VALUE)).is_some());
        // 100 -> 98.5: delta=1.5 > 1.0 → pass, last=98.5.
        assert!(f.apply(ev(98.5, EventMask::VALUE)).is_some());
        // 98.5 -> 98.0: delta=0.5 < 1.0 → drop.
        assert!(f.apply(ev(98.0, EventMask::VALUE)).is_none());
        // 98.5 -> 97.0: delta=1.5 > 1.0 → pass.
        assert!(f.apply(ev(97.0, EventMask::VALUE)).is_some());
    }

    /// Relative deadband: `delta > threshold * |last|`. With
    /// `threshold = 0.01` (1%) and last = 100, the band is 1.0; a
    /// 1.0-magnitude step is exactly at the band and DROPS (strict).
    #[test]
    fn relative_deadband_scales_with_last_value() {
        let f = DeadbandFilter::relative(0.01); // 1%
        assert!(f.apply(ev(100.0, EventMask::VALUE)).is_some());
        assert!(f.apply(ev(100.5, EventMask::VALUE)).is_none()); // 0.5 ≤ 1.0 band
        assert!(f.apply(ev(101.0, EventMask::VALUE)).is_none()); // 1.0 == band, drop
        assert!(f.apply(ev(101.5, EventMask::VALUE)).is_some()); // 1.5 > 1.0
    }

    /// When `last == 0` the relative band collapses to `0`, so any
    /// non-zero delta passes (C: `hyst = 0 * cval/100 = 0` → `delta > 0`).
    #[test]
    fn relative_deadband_zero_baseline_passes_any_nonzero() {
        let f = DeadbandFilter::relative(0.5);
        assert!(f.apply(ev(0.0, EventMask::VALUE)).is_some());
        assert!(
            f.apply(ev(0.0, EventMask::VALUE)).is_none(),
            "delta=0 vs band=0 → strict > drops"
        );
        assert!(
            f.apply(ev(0.1, EventMask::VALUE)).is_some(),
            "0.1 > 0 band — passes"
        );
    }

    /// 446e0d4a — `dbnd` MUST NOT gate `DBE_ALARM`. The event passes
    /// regardless of deadband. C updates `*poldval = newval` whenever
    /// `delta > deadband` — even on an alarm-only emission — so the
    /// next pure VALUE emission compares against the fresh baseline.
    #[test]
    fn alarm_events_pass_and_update_state_when_supra_threshold() {
        let f = DeadbandFilter::absolute(10.0);
        // Seed value-state with 100.
        assert!(f.apply(ev(100.0, EventMask::VALUE)).is_some());
        // 200 alone would normally pass (delta 100 > 10). Tagging
        // it as ALARM also passes (446e0d4a) AND updates last_sent
        // because the C deadband check is unconditional.
        assert!(f.apply(ev(200.0, EventMask::ALARM)).is_some());
        // A subsequent VALUE emission at 205 is now compared against
        // the updated 200, not the stale 100.
        assert!(f.apply(ev(205.0, EventMask::VALUE)).is_none());
    }

    /// Sub-threshold ALARM events still pass (and don't update state).
    #[test]
    fn alarm_events_subthreshold_pass_without_state_change() {
        let f = DeadbandFilter::absolute(10.0);
        assert!(f.apply(ev(100.0, EventMask::VALUE)).is_some());
        // delta=1 ≤ 10 → no state update, but ALARM bit forces pass.
        assert!(f.apply(ev(101.0, EventMask::ALARM)).is_some());
        // last_sent untouched (still 100) — a fresh 101 VALUE is
        // silenced (delta=1 ≤ 10).
        assert!(f.apply(ev(101.0, EventMask::VALUE)).is_none());
    }

    /// Same rule for `DBE_PROPERTY` — sub-threshold pass, no state change.
    #[test]
    fn property_events_pass_without_updating_state_when_subthreshold() {
        let f = DeadbandFilter::absolute(10.0);
        assert!(f.apply(ev(50.0, EventMask::VALUE)).is_some());
        assert!(f.apply(ev(50.5, EventMask::PROPERTY)).is_some());
        // last_sent untouched — 50.5 still inside the deadband around 50.
        assert!(f.apply(ev(50.5, EventMask::VALUE)).is_none());
    }

    /// NaN ↔ finite transitions produce delta=INF in C; they MUST
    /// trip the deadband (`recGblCheckDeadbandTest.c` test 4, 7).
    #[test]
    fn nan_to_finite_transition_passes() {
        let f = DeadbandFilter::absolute(1.5);
        // Seed with NaN first.
        assert!(
            f.apply(ev(f64::NAN, EventMask::VALUE)).is_none(),
            "NaN→NaN seed: delta=0, drops (recGblCheckDeadbandTest test 8)"
        );
        // First finite event with NaN seed: delta=INF → passes.
        assert!(f.apply(ev(1.0, EventMask::VALUE)).is_some());
        // Then NaN: delta=INF → passes, last=NaN.
        assert!(f.apply(ev(f64::NAN, EventMask::VALUE)).is_some());
        // Finite again: delta=INF → passes.
        assert!(f.apply(ev(2.0, EventMask::VALUE)).is_some());
    }

    /// +inf vs -inf is INF delta in C — passes.
    #[test]
    fn plus_inf_to_minus_inf_passes() {
        let f = DeadbandFilter::absolute(1.5);
        assert!(f.apply(ev(f64::INFINITY, EventMask::VALUE)).is_some());
        assert!(f.apply(ev(f64::NEG_INFINITY, EventMask::VALUE)).is_some());
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
