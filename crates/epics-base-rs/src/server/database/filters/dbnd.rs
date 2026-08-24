//! `dbnd` — value deadband filter (epics-base 3.15.7 channel filters).
//!
//! Suppresses value-change `MonitorEvent`s when the new value sits
//! inside a `±threshold` band around the last value the subscriber
//! actually saw. Alarm and property events always pass through —
//! epics-base 446e0d4a fixed the C `dbnd` so it never gates
//! `DBE_ALARM` / `DBE_PROPERTY` (a stale property silenced by a
//! deadband would desync clients).
//!
//! C `dbnd.c` filter wire syntax (`modeEnum {"abs","rel"}`, opts
//! `d`/`m`/`abs`/`rel`): `PV.{"dbnd":{"d":0.5}}` or `{"abs":0.5}`
//! (absolute); `PV.{"dbnd":{"rel":50}}` or `{"d":50,"m":"rel"}`
//! (relative, percent of last value). There is no `r` key.
//!
//! ## Deviation from C: the deadband is a magnitude (signed-off)
//!
//! C compares `delta = |last - new|` against a *signed* deadband
//! (`recGblCheckDeadband`, recGbl.c:345-370 — `if (delta > deadband)`,
//! with no `abs()` on `deadband`). In relative mode C refreshes
//! `hyst = val * cval/100` after each delivered event (dbnd.c:87), so a
//! positive percentage on a *negative* PV value drives `hyst` negative:
//! every finite `delta` then satisfies `delta > hyst`, the deadband is
//! effectively disabled, and the subscriber is flooded with events.
//!
//! This port intentionally treats the deadband as a magnitude — `cval`
//! is stored as `|cval|` and the refreshed band as `|val * cval/100|` —
//! so a negative-valued channel is suppressed
//! exactly like a positive-valued one. The cost is a deliberate parity
//! deviation: under epics-rs a `{"rel":N}` filter on a negative-baseline
//! PV emits *fewer* events than C (which emits all of them), and a
//! negative configured `d` is normalised to its magnitude instead of
//! inverting the comparison the way C's signed double would. Absolute
//! mode is unaffected for the usual non-negative threshold. `DeadbandFilter`
//! is the sole owner of this rule; it is not shared with record MDEL/ADEL
//! processing (`record_instance::ln`).

use parking_lot::Mutex;

use super::{FilteredMonitorEvent, SubscriptionFilter};
use crate::server::recgbl::EventMask;
use crate::types::EpicsValue;

/// Deadband mode — absolute or relative-to-last-value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeadbandMode {
    /// `|new - last| > threshold` passes. The mode for the `d` key
    /// (default), the `abs` key, and `d` with `m:"abs"`. Comparison is
    /// strict — matches C `recGblCheckDeadband` (`if (delta > deadband)`).
    Absolute,
    /// The band is a percentage of the value at the last delivery, held
    /// as its own state: after every delivered event C recomputes
    /// `hyst = val * cval/100` (`dbnd.c:87`) and compares the NEXT delta
    /// against that. The mode for the `rel` key and `d` with `m:"rel"`;
    /// `cval` is C's percent, so `{"rel":50}` is 50%. When the delivered
    /// value is 0 the band becomes 0 and any non-zero delta passes.
    Relative,
}

/// C `dbnd.c`'s `myStruct` state, behind one lock so `last` and `hyst`
/// can never be read from different moments.
struct DeadbandState {
    /// C `myStruct::last` — the baseline `recGblCheckDeadband` compares
    /// against and advances whenever `delta > deadband`. Seeded `NaN` by
    /// `parse_ok` (`dbnd.c:61`) so the first finite event produces
    /// `delta = INF` and always passes.
    last: f64,
    /// C `myStruct::hyst` — the band ITSELF, and its own state rather
    /// than a function of `last`. `parse_ok` seeds it to `cval`
    /// (`dbnd.c:60`) and the filter refreshes it to `val * cval/100`
    /// after ANY event that was sent (`dbnd.c:86-88`), including one sent
    /// only because it carried `DBE_ALARM` or `DBE_PROPERTY`. Deriving
    /// the band from `last` instead misses exactly those refreshes,
    /// because a bypassed event leaves `last` untouched.
    hyst: f64,
}

/// `dbnd` filter.
pub struct DeadbandFilter {
    /// C `myStruct::cval` — the deadband as the wire carries it: an
    /// absolute magnitude in `abs` mode, a PERCENT in `rel` mode. Kept in
    /// the wire form because C's `hyst` refresh is written in it
    /// (`val * cval/100`).
    cval: f64,
    mode: DeadbandMode,
    state: Mutex<DeadbandState>,
}

impl DeadbandFilter {
    /// `cval` is C's wire value — an absolute magnitude for
    /// [`DeadbandMode::Absolute`], a percent for
    /// [`DeadbandMode::Relative`].
    pub fn new(cval: f64, mode: DeadbandMode) -> Self {
        // Magnitude deadband — intentional, signed-off deviation from
        // C's signed `recGblCheckDeadband` comparison. See the module
        // doc "Deviation from C" for the rationale.
        let cval = cval.abs();
        Self {
            cval,
            mode,
            state: Mutex::new(DeadbandState {
                last: f64::NAN,
                // C `parse_ok`: `my->hyst = my->cval` for both modes
                // (`dbnd.c:60`). In `rel` mode that is the raw percent,
                // which only ever gates the first event — and the first
                // event's delta is INF against the NaN baseline anyway.
                hyst: cval,
            }),
        }
    }

    /// Convenience: absolute-deadband filter with the given threshold.
    pub fn absolute(threshold: f64) -> Self {
        Self::new(threshold, DeadbandMode::Absolute)
    }

    /// Convenience: relative-deadband filter, in C's percent units
    /// (`1.0` = 1%).
    pub fn relative(percent: f64) -> Self {
        Self::new(percent, DeadbandMode::Relative)
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
        // deadband semantic — pass unconditionally and don't touch the
        // state. C reaches this by way of `pfl->type != dbfl_type_val`
        // and its `send = 1` initialiser (`dbnd.c:69, 75`).
        let Some(cur) = event.event.snapshot.value.to_f64() else {
            return Some(event);
        };

        let mut state = self.state.lock();
        let delta = c_delta(state.last, cur);
        // C `recGblCheckDeadband`: `if (delta > deadband)` — strict, and
        // against `my->hyst`, which in `rel` mode was last set from the
        // value at the previous SEND, not from `last`.
        let supra = delta > state.hyst;
        if supra {
            // C updates `*poldval = newval` whenever `delta > deadband`,
            // regardless of which mask bits are set. The same write must
            // happen here even when the event is delivered solely because
            // of an ALARM / PROPERTY bit (446e0d4a) so a subsequent
            // VALUE-only emission is compared against the right baseline.
            state.last = cur;
        }

        // C `dbnd.c:84`: `send = pfl->mask & ~(DBE_VALUE|DBE_LOG)` —
        // both VALUE and LOG are stripped before the deadband test, and
        // `recGblCheckDeadband` re-adds `mask & (DBE_VALUE|DBE_LOG)` only
        // when `delta > deadband`. So VALUE *and* LOG are deadband-gated;
        // only ALARM / PROPERTY (446e0d4a) bypass the deadband. Stripping
        // LOG here too is what stops the widened `VALUE|LOG` emission
        // mask from defeating the `.{dbnd}` filter on every value change.
        let bypass = EventMask::from_bits(
            event.event.mask.bits() & !(EventMask::VALUE | EventMask::LOG).bits(),
        );
        let send = supra || !bypass.is_empty();
        if send && self.mode == DeadbandMode::Relative {
            // C `dbnd.c:86-88`: `if (send && my->mode == 1) my->hyst =
            // val * my->cval/100.` — keyed on `send`, so an event that
            // only passed on its ALARM / PROPERTY bit still moves the
            // band. `.abs()` is the module's signed-off magnitude
            // deviation, applied here rather than to the comparison.
            state.hyst = (cur * self.cval / 100.0).abs();
        }
        if send { Some(event) } else { None }
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

    /// Relative deadband: the band is refreshed to `val * cval/100` after
    /// each delivery. With `cval = 1` (1%) and a delivered 100, the band
    /// is 1.0; a 1.0-magnitude step is exactly at the band and DROPS
    /// (strict).
    #[test]
    fn relative_deadband_scales_with_last_value() {
        let f = DeadbandFilter::relative(1.0); // 1%
        assert!(f.apply(ev(100.0, EventMask::VALUE)).is_some());
        assert!(f.apply(ev(100.5, EventMask::VALUE)).is_none()); // 0.5 ≤ 1.0 band
        assert!(f.apply(ev(101.0, EventMask::VALUE)).is_none()); // 1.0 == band, drop
        assert!(f.apply(ev(101.5, EventMask::VALUE)).is_some()); // 1.5 > 1.0
    }

    /// C keys the band refresh on `send`, not on `delta > deadband`
    /// (`dbnd.c:86-88`), so an event delivered ONLY because it carried
    /// `DBE_ALARM` still moves the band to a percentage of ITS value
    /// while leaving `last` where it was. Deriving the band from `last`
    /// misses that refresh and delivers an update C suppresses.
    #[test]
    fn relative_band_is_refreshed_by_an_alarm_bypass_that_left_last_alone() {
        let f = DeadbandFilter::relative(50.0); // 50%
        // Delivered: last = 10, hyst = 10 * 50/100 = 5.
        assert!(f.apply(ev(10.0, EventMask::VALUE)).is_some());
        // delta = 2 <= 5, so `last` stays 10 — but the ALARM bit sends the
        // event, and C therefore refreshes hyst to 12 * 50/100 = 6.
        assert!(
            f.apply(ev(12.0, EventMask::VALUE | EventMask::ALARM))
                .is_some(),
            "the alarm class bypasses the deadband (446e0d4a)"
        );
        // delta from last = 10 is 5.5: under the band C now holds (6), and
        // over the 5 a `last`-derived band would still be using.
        assert!(
            f.apply(ev(15.5, EventMask::VALUE)).is_none(),
            "5.5 <= the refreshed band of 6, so C sends nothing"
        );
        // 7.5 clears the refreshed band.
        assert!(f.apply(ev(17.5, EventMask::VALUE)).is_some());
    }

    /// When `last == 0` the relative band collapses to `0`, so any
    /// non-zero delta passes (C: `hyst = 0 * cval/100 = 0` → `delta > 0`).
    #[test]
    fn relative_deadband_zero_baseline_passes_any_nonzero() {
        let f = DeadbandFilter::relative(50.0); // 50%
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

    /// Regression — the value-change emission mask was widened
    /// to `VALUE|LOG` (C-faithful, gateVc.cc posts VALUE|ALARM|LOG).
    /// dbnd MUST strip LOG as well as VALUE before the deadband test
    /// (C `dbnd.c:84`: `send = pfl->mask & ~(DBE_VALUE|DBE_LOG)`);
    /// otherwise the LOG bit makes the bypass mask non-empty and every
    /// value change defeats the `.{dbnd}` filter. A sub-threshold
    /// `VALUE|LOG` change and a sub-threshold LOG-only event must both
    /// be suppressed; a supra-threshold change is delivered.
    #[test]
    fn value_log_subthreshold_is_deadband_gated() {
        let f = DeadbandFilter::absolute(1.0);
        // Seed baseline at 10.0 (delta=INF on NaN seed → passes).
        assert!(
            f.apply(ev(10.0, EventMask::VALUE | EventMask::LOG))
                .is_some()
        );
        // Sub-threshold VALUE|LOG (delta 0.4 ≤ 1.0): LOG must NOT
        // bypass the deadband — suppressed.
        assert!(
            f.apply(ev(10.4, EventMask::VALUE | EventMask::LOG))
                .is_none(),
            "VALUE|LOG sub-threshold change must be deadband-gated (C dbnd.c:84 strips DBE_LOG)"
        );
        // LOG-only sub-threshold event is likewise gated (last_sent
        // untouched, still 10.0; delta 0.4 ≤ 1.0).
        assert!(
            f.apply(ev(10.4, EventMask::LOG)).is_none(),
            "LOG-only sub-threshold event must be deadband-gated"
        );
        // Supra-threshold VALUE|LOG (delta 1.5 > 1.0) is delivered.
        assert!(
            f.apply(ev(11.5, EventMask::VALUE | EventMask::LOG))
                .is_some(),
            "VALUE|LOG supra-threshold change passes"
        );
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
        let event = FilteredMonitorEvent::new(MonitorEvent {
            snapshot: std::sync::Arc::new(snap),
            origin: 0,
            mask: EventMask::VALUE,
        });
        assert!(f.apply(event).is_some());
    }
}
