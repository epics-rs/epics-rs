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
//! Alarm and property events always pass through (446e0d4a) and do
//! NOT consume a decimation slot — they are out-of-band signals.
//!
//! pvxs `decimate.cpp` clamps `n < 1` to `1` (no decimation), which we
//! match. The decimator stores its position counter inside a small
//! `Mutex<u64>` so the same filter handle can be shared between
//! subscribers without ABA hazards on the counter.

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
        // 446e0d4a: alarm / property events pass without consuming
        // a decimation slot. Otherwise an alarm-only emission could
        // shift the counter and silence the next value update that
        // would otherwise have fired.
        if !event.mask.contains(EventMask::VALUE) {
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
        FilteredMonitorEvent::new(
            MonitorEvent {
                snapshot: Snapshot::new(EpicsValue::Double(v), 0, 0, SystemTime::UNIX_EPOCH),
                origin: 0,
            },
            mask,
        )
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

    /// Alarm-only emissions pass through and DO NOT consume a slot.
    #[test]
    fn alarm_passes_without_consuming_slot() {
        let f = DecimateFilter::every(3);
        // 1st value: pass.
        assert!(f.apply(ev(0.0, EventMask::VALUE)).is_some());
        // Alarm: pass, counter unchanged.
        assert!(f.apply(ev(0.0, EventMask::ALARM)).is_some());
        assert!(f.apply(ev(0.0, EventMask::ALARM)).is_some());
        // Next two value events drop (positions 1, 2 of the window).
        assert!(f.apply(ev(1.0, EventMask::VALUE)).is_none());
        assert!(f.apply(ev(2.0, EventMask::VALUE)).is_none());
        // The 4th value event passes (position 0 of next window).
        assert!(f.apply(ev(3.0, EventMask::VALUE)).is_some());
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
