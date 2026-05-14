//! `arr` — array-slice filter (epics-base 3.15.7 channel filters).
//!
//! Operates on array snapshots (`*Array` `EpicsValue` variants):
//! emits a slice `[start, end]` taken at stride `incr`. Mirrors the
//! pvxs / dbAccess `arr` filter JSON schema:
//!
//! ```text
//! PV.{"arr":{"s":start,"i":incr,"e":end}}
//! ```
//!
//! Semantics (matching epics-base `arr.c`):
//! * `start` / `end` default to `0` / `-1` (full array). Negative
//!   indices count from the end (`-1` == last element).
//! * `incr` defaults to `1`. Must be ≥ 1; values < 1 are treated as 1.
//! * Resulting slice carries the same array variant as the input.
//! * Scalar (non-array) values pass through unchanged.
//! * Slicing is unconditional regardless of `mask` — `arr` is a
//!   transformation filter (`channelRegisterPost`), not a gate. The
//!   446e0d4a fix applies only to value-gating filters (`dbnd`); an
//!   alarm-tagged emission carrying the array value MUST be sliced
//!   so the client receives a coherent slice-view.

use parking_lot::Mutex;

use super::{FilteredMonitorEvent, SubscriptionFilter};
use crate::types::EpicsValue;

pub struct ArrayFilter {
    config: ArrayFilterConfig,
    // Filter is stateless aside from this immutable config; the
    // Mutex<()> guards against future state additions without
    // changing the public API.
    _state: Mutex<()>,
}

#[derive(Debug, Clone, Copy)]
pub struct ArrayFilterConfig {
    pub start: i64,
    pub incr: i64,
    pub end: i64,
}

impl Default for ArrayFilterConfig {
    fn default() -> Self {
        Self {
            start: 0,
            incr: 1,
            end: -1,
        }
    }
}

impl ArrayFilter {
    pub fn new(config: ArrayFilterConfig) -> Self {
        let incr = config.incr.max(1);
        Self {
            config: ArrayFilterConfig { incr, ..config },
            _state: Mutex::new(()),
        }
    }
}

impl SubscriptionFilter for ArrayFilter {
    fn name(&self) -> &'static str {
        "arr"
    }

    fn apply(&self, event: FilteredMonitorEvent) -> Option<FilteredMonitorEvent> {
        let cfg = self.config;
        let mut event = event;
        event.event.snapshot.value = match event.event.snapshot.value {
            EpicsValue::ShortArray(v) => EpicsValue::ShortArray(slice_with(v, cfg)),
            EpicsValue::LongArray(v) => EpicsValue::LongArray(slice_with(v, cfg)),
            EpicsValue::Int64Array(v) => EpicsValue::Int64Array(slice_with(v, cfg)),
            EpicsValue::FloatArray(v) => EpicsValue::FloatArray(slice_with(v, cfg)),
            EpicsValue::DoubleArray(v) => EpicsValue::DoubleArray(slice_with(v, cfg)),
            EpicsValue::EnumArray(v) => EpicsValue::EnumArray(slice_with(v, cfg)),
            EpicsValue::CharArray(v) => EpicsValue::CharArray(slice_with(v, cfg)),
            EpicsValue::StringArray(v) => EpicsValue::StringArray(slice_with(v, cfg)),
            other => other, // scalar — pass through
        };
        Some(event)
    }
}

/// Apply `start..=end` (negative indices wrap from `len`) with stride
/// `incr`. Returns a fresh `Vec` whenever the slice is non-trivial.
/// Mirrors C `arr.c::wrapArrayIndices` — note the asymmetric clamps:
/// `start` clamps to `[0, len]` (one past last) while `end` clamps to
/// `[0, len-1]`, so a `start > len-1` request resolves to `start > end`
/// and yields 0 elements (not 1).
fn slice_with<T: Clone>(input: Vec<T>, cfg: ArrayFilterConfig) -> Vec<T> {
    let len = input.len() as i64;
    if len == 0 {
        return input;
    }
    let resolve_start = |idx: i64| -> i64 {
        let r = if idx < 0 { len + idx } else { idx };
        r.clamp(0, len)
    };
    let resolve_end = |idx: i64| -> i64 {
        let r = if idx < 0 { len + idx } else { idx };
        r.clamp(0, len - 1)
    };
    let start = resolve_start(cfg.start);
    let end = resolve_end(cfg.end);
    if start > end {
        return Vec::new();
    }
    let mut out = Vec::with_capacity(((end - start) / cfg.incr + 1) as usize);
    let mut idx = start;
    while idx <= end {
        out.push(input[idx as usize].clone());
        idx += cfg.incr;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::pv::MonitorEvent;
    use crate::server::recgbl::EventMask;
    use crate::server::snapshot::Snapshot;
    use std::time::SystemTime;

    fn ev_array(v: Vec<f64>) -> FilteredMonitorEvent {
        FilteredMonitorEvent::new(
            MonitorEvent {
                snapshot: Snapshot::new(EpicsValue::DoubleArray(v), 0, 0, SystemTime::UNIX_EPOCH),
                origin: 0,
            },
            EventMask::VALUE,
        )
    }

    fn unpack(event: FilteredMonitorEvent) -> Vec<f64> {
        match event.event.snapshot.value {
            EpicsValue::DoubleArray(v) => v,
            other => panic!("expected DoubleArray, got {other:?}"),
        }
    }

    /// Default config (`s=0, i=1, e=-1`) yields the full array.
    #[test]
    fn default_config_is_identity() {
        let f = ArrayFilter::new(ArrayFilterConfig::default());
        let out = f.apply(ev_array(vec![1.0, 2.0, 3.0, 4.0])).unwrap();
        assert_eq!(unpack(out), vec![1.0, 2.0, 3.0, 4.0]);
    }

    /// Positive slice indices.
    #[test]
    fn positive_indices_select_range() {
        let f = ArrayFilter::new(ArrayFilterConfig {
            start: 1,
            incr: 1,
            end: 3,
        });
        let out = f.apply(ev_array(vec![0.0, 1.0, 2.0, 3.0, 4.0])).unwrap();
        assert_eq!(unpack(out), vec![1.0, 2.0, 3.0]);
    }

    /// Negative end index counts from the end (`-1` is last).
    #[test]
    fn negative_end_counts_from_back() {
        let f = ArrayFilter::new(ArrayFilterConfig {
            start: 0,
            incr: 1,
            end: -2, // up to second-to-last
        });
        let out = f.apply(ev_array(vec![0.0, 1.0, 2.0, 3.0, 4.0])).unwrap();
        assert_eq!(unpack(out), vec![0.0, 1.0, 2.0, 3.0]);
    }

    /// Stride > 1 picks every Nth element.
    #[test]
    fn stride_picks_every_nth() {
        let f = ArrayFilter::new(ArrayFilterConfig {
            start: 0,
            incr: 2,
            end: -1,
        });
        let out = f.apply(ev_array(vec![0.0, 1.0, 2.0, 3.0, 4.0])).unwrap();
        assert_eq!(unpack(out), vec![0.0, 2.0, 4.0]);
    }

    /// `incr < 1` is clamped to 1 (the pvxs rule).
    #[test]
    fn invalid_incr_clamps_to_one() {
        let f = ArrayFilter::new(ArrayFilterConfig {
            start: 0,
            incr: 0,
            end: -1,
        });
        let out = f.apply(ev_array(vec![1.0, 2.0])).unwrap();
        assert_eq!(unpack(out), vec![1.0, 2.0]);
    }

    /// Empty array passes through (no slicing to do, no panic).
    #[test]
    fn empty_array_passes_through() {
        let f = ArrayFilter::new(ArrayFilterConfig::default());
        let out = f.apply(ev_array(Vec::new())).unwrap();
        assert!(unpack(out).is_empty());
    }

    /// Out-of-order range (start > end after resolution) yields empty.
    #[test]
    fn inverted_range_yields_empty() {
        let f = ArrayFilter::new(ArrayFilterConfig {
            start: 3,
            incr: 1,
            end: 1,
        });
        let out = f.apply(ev_array(vec![0.0, 1.0, 2.0, 3.0, 4.0])).unwrap();
        assert!(unpack(out).is_empty());
    }

    /// Scalar values pass unchanged.
    #[test]
    fn scalar_passes_unchanged() {
        let f = ArrayFilter::new(ArrayFilterConfig::default());
        let ev = FilteredMonitorEvent::new(
            MonitorEvent {
                snapshot: Snapshot::new(EpicsValue::Double(3.14), 0, 0, SystemTime::UNIX_EPOCH),
                origin: 0,
            },
            EventMask::VALUE,
        );
        let out = f.apply(ev).unwrap();
        assert!(
            matches!(out.event.snapshot.value, EpicsValue::Double(v) if (v - 3.14).abs() < 1e-9)
        );
    }

    /// Alarm events are ALSO sliced — `arr` is a transformation
    /// filter (C `channelRegisterPost`), not a value gate. 446e0d4a
    /// applies to `dbnd` only.
    #[test]
    fn alarm_event_is_also_sliced() {
        let f = ArrayFilter::new(ArrayFilterConfig {
            start: 0,
            incr: 1,
            end: 0, // slice to single element
        });
        let mut ev = ev_array(vec![1.0, 2.0, 3.0]);
        ev.mask = EventMask::ALARM;
        let out = f.apply(ev).unwrap();
        assert_eq!(unpack(out), vec![1.0]);
    }

    /// Out-of-range start (greater than `len-1`) clamps to `len` per
    /// C `wrapArrayIndices`, so `start > end` and the slice is empty.
    /// Without the asymmetric start/end clamp the resolved indices
    /// collapse to `len-1` and the slice incorrectly returns 1 element.
    #[test]
    fn start_beyond_len_yields_empty() {
        let f = ArrayFilter::new(ArrayFilterConfig {
            start: 10,
            incr: 1,
            end: -1,
        });
        let out = f.apply(ev_array(vec![1.0, 2.0, 3.0])).unwrap();
        assert!(
            unpack(out).is_empty(),
            "C parity: start>len returns 0 elements"
        );
    }
}
