//! `decimate` — N-to-1 event decimator (epics-base 3.15.7 channel filters).
//!
//! Passes the first value event of each `N`-sized window and silently
//! drops the rest — C `decimate.c:58-77`, whose counter starts at 0 and
//! forwards on `i++ == 0`.
//!
//! JSON syntax: `PV.{"dec":{"n":N}}`. `n` is the only key `decimate.c`'s
//! `chfPluginArgDef opts[]` defines, so any other key fails channel
//! creation at the parser's `parse_map_key` stage.
//!
//! Per epics-base `decimate.c` only `DBE_PROPERTY` events bypass the
//! decimator (`if (pfl->mask & DBE_PROPERTY) return pfl`). `DBE_ALARM`
//! emissions DO consume a slot and may be dropped — the 446e0d4a
//! "alarm always passes" rule is dbnd-specific.
//!
//! The decimator stores its position counter inside a small `Mutex<u64>`
//! so the same filter handle can be shared between subscribers without
//! ABA hazards on the counter.

use parking_lot::Mutex;

use super::{FilteredMonitorEvent, SubscriptionFilter};
use crate::server::recgbl::EventMask;

pub struct DecimateFilter {
    n: u64,
    counter: Mutex<u64>,
}

impl DecimateFilter {
    /// `None` for `n < 1`: that is `decimate.c`'s own `parse_ok`
    /// (`decimate.c:49-56`, `return -1`), which `chfPlugin` turns into
    /// `parse_stop` and `dbChannelCreate` into a channel-creation
    /// failure. Answering `None` from the constructor is what keeps a
    /// caller from clamping the illegal value into a legal one instead.
    pub fn new(n: u64) -> Option<Self> {
        (n >= 1).then(|| Self {
            n,
            counter: Mutex::new(0),
        })
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
        if position == 0 { Some(event) } else { None }
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

    /// `n=3` passes the 1st, 4th, 7th, ...
    #[test]
    fn every_n_passes_first_of_each_window() {
        let f = DecimateFilter::new(3).unwrap();
        let kept: Vec<bool> = (0..7)
            .map(|i| f.apply(ev(i as f64, EventMask::VALUE)).is_some())
            .collect();
        assert_eq!(kept, vec![true, false, false, true, false, false, true]);
    }

    /// `n=1` is a pass-through.
    #[test]
    fn n_of_one_passes_everything() {
        let f = DecimateFilter::new(1).unwrap();
        for i in 0..5 {
            assert!(f.apply(ev(i as f64, EventMask::VALUE)).is_some());
        }
    }

    /// `n=0` is rejected, not clamped — C `decimate.c`'s `parse_ok`
    /// returns -1 and the channel is never created.
    #[test]
    fn n_of_zero_is_rejected() {
        assert!(DecimateFilter::new(0).is_none());
    }

    /// `DBE_PROPERTY` events bypass the decimator unconditionally
    /// — matches C `decimate.c`'s `if (pfl->mask & DBE_PROPERTY)
    /// return pfl` short-circuit.
    #[test]
    fn property_bypasses_decimator() {
        let f = DecimateFilter::new(3).unwrap();
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
        let f = DecimateFilter::new(3).unwrap();
        assert!(f.apply(ev(0.0, EventMask::VALUE)).is_some()); // pos 0 — pass
        // pos 1 — ALARM gets DROPPED (counter advances).
        assert!(f.apply(ev(0.0, EventMask::ALARM)).is_none());
        // pos 2 — VALUE dropped.
        assert!(f.apply(ev(1.0, EventMask::VALUE)).is_none());
        // pos 3 ≡ 0 — pass.
        assert!(f.apply(ev(2.0, EventMask::VALUE)).is_some());
    }
}
