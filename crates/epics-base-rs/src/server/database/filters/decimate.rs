//! `decimate` — N-to-1 event decimator (epics-base 3.15.7 channel filters).
//!
//! Passes every Nth value event; the rest are silently dropped. The
//! offset (`offset`, default 0) controls which member of each `N`-sized
//! window is forwarded:
//!
//! * `offset = 0` (default) — first event of each window.
//! * `offset = N-1` — last event of each window (acts like an
//!   "every-N" trigger).
//!
//! pvxs JSON syntax: `PV.{"dec":{"n":N,"offset":K}}`.
//!
//! Per epics-base `decimate.c` only `DBE_PROPERTY` events bypass the
//! decimator (`if (pfl->mask & DBE_PROPERTY) return pfl`). `DBE_ALARM`
//! emissions DO consume a slot and may be dropped — the 446e0d4a
//! "alarm always passes" rule is dbnd-specific. The Rust `offset`
//! parameter is a Rust extension (C `decimate.c` only accepts `n`).
//!
//! C `parse_ok` rejects `n < 1` outright; we clamp to 1 (no
//! decimation) to keep client subscriptions alive rather than tearing
//! them down on a bad config. The decimator stores its position
//! counter inside a small `Mutex<u64>` so the same filter handle can
//! be shared between subscribers without ABA hazards on the counter.

use parking_lot::Mutex;

use super::{FilteredMonitorEvent, SubscriptionFilter};
use crate::server::recgbl::EventMask;

pub struct DecimateFilter {
    n: u64,
    offset: u64,
    counter: Mutex<u64>,
}

impl DecimateFilter {
    pub fn new(n: u64, offset: u64) -> Self {
        let n = n.max(1);
        let offset = offset % n;
        Self {
            n,
            offset,
            counter: Mutex::new(0),
        }
    }

    /// Convenience: pass every Nth event starting from the first.
    pub fn every(n: u64) -> Self {
        Self::new(n, 0)
    }
}

impl SubscriptionFilter for DecimateFilter {
    fn name(&self) -> &'static str {
        "dec"
    }

    fn apply(&self, event: FilteredMonitorEvent) -> Option<FilteredMonitorEvent> {
        // C `decimate.c:64`: `if (pfl->ctx == dbfl_context_read ||
        // (pfl->mask & DBE_PROPERTY)) return pfl;` — a single-read
        // emission and a `DBE_PROPERTY` event both bypass the counter
        // unchanged. `DBE_ALARM` runs through the decimation logic
        // and may be dropped.
        if event.read_context || event.event.mask.intersects(EventMask::PROPERTY) {
            return Some(event);
        }
        let mut counter = self.counter.lock();
        let position = *counter % self.n;
        *counter = counter.wrapping_add(1);
        if position == self.offset {
            Some(event)
        } else {
            None
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

    /// `every(3)` passes the 1st, 4th, 7th, ...
    #[test]
    fn every_n_passes_first_of_each_window() {
        let f = DecimateFilter::every(3);
        let kept: Vec<bool> = (0..7)
            .map(|i| f.apply(ev(i as f64, EventMask::VALUE)).is_some())
            .collect();
        assert_eq!(kept, vec![true, false, false, true, false, false, true]);
    }

    /// `n=1` is a pass-through.
    #[test]
    fn n_of_one_passes_everything() {
        let f = DecimateFilter::every(1);
        for i in 0..5 {
            assert!(f.apply(ev(i as f64, EventMask::VALUE)).is_some());
        }
    }

    /// `n=0` clamps to 1 (no decimation).
    #[test]
    fn n_of_zero_clamps_to_one() {
        let f = DecimateFilter::new(0, 0);
        for i in 0..3 {
            assert!(f.apply(ev(i as f64, EventMask::VALUE)).is_some());
        }
    }

    /// `offset = N-1` shifts the window so the LAST of each window passes.
    #[test]
    fn offset_picks_last_of_window() {
        let f = DecimateFilter::new(3, 2);
        let kept: Vec<bool> = (0..6)
            .map(|i| f.apply(ev(i as f64, EventMask::VALUE)).is_some())
            .collect();
        assert_eq!(kept, vec![false, false, true, false, false, true]);
    }

    /// `DBE_PROPERTY` events bypass the decimator unconditionally
    /// — matches C `decimate.c`'s `if (pfl->mask & DBE_PROPERTY)
    /// return pfl` short-circuit.
    #[test]
    fn property_bypasses_decimator() {
        let f = DecimateFilter::every(3);
        assert!(f.apply(ev(0.0, EventMask::VALUE)).is_some()); // 1st VALUE: pass
        // PROPERTY events pass without consuming a slot.
        assert!(f.apply(ev(0.0, EventMask::PROPERTY)).is_some());
        assert!(f.apply(ev(0.0, EventMask::PROPERTY)).is_some());
        // Next two VALUE events drop (positions 1, 2 of the window).
        assert!(f.apply(ev(1.0, EventMask::VALUE)).is_none());
        assert!(f.apply(ev(2.0, EventMask::VALUE)).is_none());
        // The 4th VALUE event passes (position 0 of next window).
        assert!(f.apply(ev(3.0, EventMask::VALUE)).is_some());
    }

    /// `DBE_ALARM` events DO consume a decimation slot — C
    /// `decimate.c` only special-cases PROPERTY. An alarm at
    /// position 1 of a `every(3)` window is dropped, not bypassed.
    #[test]
    fn alarm_consumes_a_slot() {
        let f = DecimateFilter::every(3);
        assert!(f.apply(ev(0.0, EventMask::VALUE)).is_some()); // pos 0 — pass
        // pos 1 — ALARM gets DROPPED (counter advances).
        assert!(f.apply(ev(0.0, EventMask::ALARM)).is_none());
        // pos 2 — VALUE dropped.
        assert!(f.apply(ev(1.0, EventMask::VALUE)).is_none());
        // pos 3 ≡ 0 — pass.
        assert!(f.apply(ev(2.0, EventMask::VALUE)).is_some());
    }

    /// `offset >= n` is folded into the valid range modulo n.
    #[test]
    fn offset_modulo_n() {
        let f = DecimateFilter::new(3, 5); // 5 % 3 = 2
        let kept: Vec<bool> = (0..6)
            .map(|i| f.apply(ev(i as f64, EventMask::VALUE)).is_some())
            .collect();
        assert_eq!(kept, vec![false, false, true, false, false, true]);
    }
}
