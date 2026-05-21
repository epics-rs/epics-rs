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
    /// Stride. **Private on purpose**: `slice_with`/`slice_len` divide by
    /// it and step a loop by it, so `incr <= 0` would divide-by-zero or
    /// loop forever. The `incr >= 1` invariant therefore holds *by
    /// construction* — the only ways to build an `ArrayFilterConfig`
    /// ([`ArrayFilterConfig::new`] and [`Default`]) clamp it to `>= 1`
    /// (the pvxs `arr` rule: `i < 1` is treated as `1`). Read it via
    /// [`ArrayFilterConfig::incr`].
    incr: i64,
    pub end: i64,
}

impl ArrayFilterConfig {
    /// Build a config, clamping `incr` to `>= 1` (pvxs `arr` rule). This
    /// is the only constructor that sets `incr`, so every reachable
    /// `ArrayFilterConfig` satisfies `incr >= 1` and the slice helpers
    /// can never divide by zero or step backwards.
    pub fn new(start: i64, incr: i64, end: i64) -> Self {
        Self {
            start,
            incr: incr.max(1),
            end,
        }
    }

    /// The stride, guaranteed `>= 1`.
    pub fn incr(&self) -> i64 {
        self.incr
    }
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
        // The `incr >= 1` invariant is owned by `ArrayFilterConfig`
        // itself (private field + clamping constructors), so there is
        // no re-clamp here — an illegal stride cannot reach this point.
        Self {
            config,
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
            EpicsValue::UInt64Array(v) => EpicsValue::UInt64Array(slice_with(v, cfg)),
            EpicsValue::FloatArray(v) => EpicsValue::FloatArray(slice_with(v, cfg)),
            EpicsValue::DoubleArray(v) => EpicsValue::DoubleArray(slice_with(v, cfg)),
            EpicsValue::EnumArray(v) => EpicsValue::EnumArray(slice_with(v, cfg)),
            EpicsValue::CharArray(v) => EpicsValue::CharArray(slice_with(v, cfg)),
            EpicsValue::StringArray(v) => EpicsValue::StringArray(slice_with(v, cfg)),
            other => other, // scalar — pass through
        };
        Some(event)
    }

    /// C `dbChannelFinalElements` parity: an `arr` slice of an
    /// `input`-element array yields `slice_len` elements. The CA
    /// CREATE_CHAN reply advertises this so the client requests (and
    /// allocates for) the sliced count instead of the unfiltered
    /// native count — without it a filtered read over-requests and the
    /// server zero-pads the slice back up to the native count.
    fn final_element_count(&self, input: usize) -> usize {
        slice_len(input as i64, self.config)
    }
}

/// Number of elements an `len`-element input produces under `cfg`,
/// without materialising the slice. Shares the asymmetric start/end
/// clamps with [`slice_with`] (C `arr.c::wrapArrayIndices`): `start`
/// clamps to `[0, len]`, `end` to `[0, len-1]`, so `start > end`
/// yields 0.
fn slice_len(len: i64, cfg: ArrayFilterConfig) -> usize {
    if len <= 0 {
        return 0;
    }
    let resolve = |idx: i64, hi: i64| -> i64 {
        let r = if idx < 0 { len + idx } else { idx };
        r.clamp(0, hi)
    };
    let start = resolve(cfg.start, len);
    let end = resolve(cfg.end, len - 1);
    if start > end {
        return 0;
    }
    ((end - start) / cfg.incr() + 1) as usize
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
    let mut out = Vec::with_capacity(((end - start) / cfg.incr() + 1) as usize);
    let mut idx = start;
    while idx <= end {
        out.push(input[idx as usize].clone());
        idx += cfg.incr();
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
        let f = ArrayFilter::new(ArrayFilterConfig::new(1, 1, 3));
        let out = f.apply(ev_array(vec![0.0, 1.0, 2.0, 3.0, 4.0])).unwrap();
        assert_eq!(unpack(out), vec![1.0, 2.0, 3.0]);
    }

    /// Negative end index counts from the end (`-1` is last).
    #[test]
    fn negative_end_counts_from_back() {
        let f = ArrayFilter::new(ArrayFilterConfig::new(0, 1, -2));
        let out = f.apply(ev_array(vec![0.0, 1.0, 2.0, 3.0, 4.0])).unwrap();
        assert_eq!(unpack(out), vec![0.0, 1.0, 2.0, 3.0]);
    }

    /// Stride > 1 picks every Nth element.
    #[test]
    fn stride_picks_every_nth() {
        let f = ArrayFilter::new(ArrayFilterConfig::new(0, 2, -1));
        let out = f.apply(ev_array(vec![0.0, 1.0, 2.0, 3.0, 4.0])).unwrap();
        assert_eq!(unpack(out), vec![0.0, 2.0, 4.0]);
    }

    /// `incr < 1` is clamped to `1` (the pvxs rule) *by construction* of
    /// `ArrayFilterConfig`, so the slice helpers cannot divide by zero or
    /// step backwards. The invariant is enforced at the type boundary,
    /// not by a separate runtime guard in `ArrayFilter::new`: a config
    /// built with `incr = 0` or a negative stride reports `incr() == 1`,
    /// and both `apply` (the divisor + the `idx += incr` loop) and
    /// `final_element_count` run without panicking.
    #[test]
    fn invalid_incr_clamps_to_one_by_construction() {
        for bad in [0_i64, -1, -7, i64::MIN] {
            let cfg = ArrayFilterConfig::new(0, bad, -1);
            assert_eq!(cfg.incr(), 1, "incr {bad} must clamp to 1 in the config");
            let f = ArrayFilter::new(cfg);
            // apply: would divide-by-zero / loop forever if incr <= 0.
            let out = f.apply(ev_array(vec![1.0, 2.0])).unwrap();
            assert_eq!(unpack(out), vec![1.0, 2.0]);
            // final_element_count: the same divisor on the count path.
            assert_eq!(f.final_element_count(2), 2);
        }
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
        let f = ArrayFilter::new(ArrayFilterConfig::new(3, 1, 1));
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
        let f = ArrayFilter::new(ArrayFilterConfig::new(0, 1, 0));
        let mut ev = ev_array(vec![1.0, 2.0, 3.0]);
        ev.mask = EventMask::ALARM;
        let out = f.apply(ev).unwrap();
        assert_eq!(unpack(out), vec![1.0]);
    }

    /// MR-R25: a `UInt64Array` waveform must be sliced like every other
    /// `*Array` variant. Before the fix the match had no `UInt64Array`
    /// arm, so DBF_UINT64 waveforms hit the scalar passthrough and the
    /// client received the full array. The values are above `i64::MAX`
    /// to prove the slice keeps both the element type and the full
    /// unsigned range.
    #[test]
    fn mr_r25_uint64_array_is_sliced() {
        let big = u64::MAX; // > i64::MAX, must survive the slice
        let f = ArrayFilter::new(ArrayFilterConfig::new(1, 1, 2));
        let ev = FilteredMonitorEvent::new(
            MonitorEvent {
                snapshot: Snapshot::new(
                    EpicsValue::UInt64Array(vec![0, big, big - 1, 7]),
                    0,
                    0,
                    SystemTime::UNIX_EPOCH,
                ),
                origin: 0,
            },
            EventMask::VALUE,
        );
        let out = f.apply(ev).unwrap();
        match out.event.snapshot.value {
            EpicsValue::UInt64Array(v) => assert_eq!(v, vec![big, big - 1]),
            other => panic!("expected sliced UInt64Array, got {other:?}"),
        }
    }

    /// `final_element_count` (C `dbChannelFinalElements`) must agree
    /// with the length of the materialised slice for the same config,
    /// across the boundary cases the CREATE_CHAN advertised count
    /// depends on.
    #[test]
    fn final_element_count_matches_slice_length() {
        let cases = [
            (ArrayFilterConfig::new(5, 1, 7), 10usize, 3usize),
            (ArrayFilterConfig::new(0, 1, -1), 10, 10), // identity
            (ArrayFilterConfig::new(0, 2, -1), 5, 3),   // stride
            (ArrayFilterConfig::new(10, 1, -1), 3, 0),  // start>len → empty
            (ArrayFilterConfig::new(3, 1, 1), 5, 0),    // inverted → empty
            (ArrayFilterConfig::default(), 0, 0),       // empty input
        ];
        for (cfg, input, expected) in cases {
            let f = ArrayFilter::new(cfg);
            assert_eq!(
                f.final_element_count(input),
                expected,
                "final_element_count({input}) for {cfg:?}"
            );
            // Cross-check against the real slice over a ramp of `input`.
            let ramp: Vec<f64> = (0..input).map(|i| i as f64).collect();
            let out = f.apply(ev_array(ramp)).unwrap();
            assert_eq!(
                unpack(out).len(),
                expected,
                "materialised slice length for {cfg:?} over {input} elements"
            );
        }
    }

    /// Out-of-range start (greater than `len-1`) clamps to `len` per
    /// C `wrapArrayIndices`, so `start > end` and the slice is empty.
    /// Without the asymmetric start/end clamp the resolved indices
    /// collapse to `len-1` and the slice incorrectly returns 1 element.
    #[test]
    fn start_beyond_len_yields_empty() {
        let f = ArrayFilter::new(ArrayFilterConfig::new(10, 1, -1));
        let out = f.apply(ev_array(vec![1.0, 2.0, 3.0])).unwrap();
        assert!(
            unpack(out).is_empty(),
            "C parity: start>len returns 0 elements"
        );
    }
}
