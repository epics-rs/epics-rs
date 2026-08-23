//! `utag` — user-tag gate (epics-base `db/std/filters/utag.c`).
//!
//! JSON syntax: `PV.{"utag":{"M":mask,"V":value}}`. An event is dropped
//! when `(utag & M) != V`. Both keys are optional; C's `allocPvt` starts
//! `mask` at `0xffffffff` and `value` at 0 (`utag.c:27-33`), so the bare
//! `{"utag":{}}` passes only events whose user tag is exactly zero.
//!
//! Three events are never dropped, in C's own order (`utag.c:45-64`):
//! a one-shot read (`dbfl_context_read`), a `DBE_PROPERTY` event, and
//! the first event the gate ever sees — `parse_ok` arms `first`
//! (`utag.c:39-43`) and the filter clears it on the first event that
//! reaches the test, so a monitor always gets its initial value.

use std::sync::atomic::{AtomicBool, Ordering};

use super::{FilteredMonitorEvent, SubscriptionFilter};
use crate::server::recgbl::EventMask;

pub struct UserTagFilter {
    mask: i32,
    value: i32,
    /// C `utagPvt::first`, armed by `parse_ok`. Cleared by the first
    /// event that reaches the drop test — reads and `DBE_PROPERTY`
    /// events return earlier and leave it armed, exactly as C does.
    first: AtomicBool,
}

impl UserTagFilter {
    /// C `allocPvt` calloc's the struct and then overwrites `mask` with
    /// `0xffffffff` (`utag.c:27-33`), so the defaults are mask = -1,
    /// value = 0.
    pub const DEFAULT_MASK: i32 = -1;

    pub fn new(mask: i32, value: i32) -> Self {
        Self {
            mask,
            value,
            first: AtomicBool::new(true),
        }
    }
}

impl SubscriptionFilter for UserTagFilter {
    fn name(&self) -> &'static str {
        "utag"
    }

    fn apply(&self, event: FilteredMonitorEvent) -> Option<FilteredMonitorEvent> {
        if event.read_context || event.event.mask.intersects(EventMask::PROPERTY) {
            return Some(event);
        }
        if self.first.swap(false, Ordering::AcqRel) {
            return Some(event);
        }
        let utag = event.event.snapshot.user_tag;
        if (utag & self.mask) != self.value {
            None
        } else {
            Some(event)
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

    fn ev(utag: i32, mask: EventMask) -> FilteredMonitorEvent {
        let mut snap = Snapshot::new(EpicsValue::Double(1.0), 0, 0, SystemTime::UNIX_EPOCH);
        snap.user_tag = utag;
        FilteredMonitorEvent::new(MonitorEvent {
            snapshot: std::sync::Arc::new(snap),
            origin: 0,
            mask,
        })
    }

    fn read_ev(utag: i32) -> FilteredMonitorEvent {
        let mut e = ev(utag, EventMask::VALUE);
        e.read_context = true;
        e
    }

    /// `(utag & M) != V` drops; the matching tag passes.
    #[test]
    fn masked_tag_gates_the_event() {
        let f = UserTagFilter::new(0x0f, 0x02);
        // First event is never dropped, whatever its tag.
        assert!(f.apply(ev(0xff, EventMask::VALUE)).is_some());
        assert!(
            f.apply(ev(0x12, EventMask::VALUE)).is_some(),
            "0x12 & 0x0f == 0x02"
        );
        assert!(
            f.apply(ev(0x13, EventMask::VALUE)).is_none(),
            "0x13 & 0x0f == 0x03"
        );
    }

    /// C `utag.c:53`: `if (pfl->ctx != dbfl_context_event || pfl->mask &
    /// DBE_PROPERTY)` — neither a read nor a property event is ever
    /// dropped, and neither consumes the `first` grace.
    #[test]
    fn reads_and_property_events_bypass_and_leave_first_armed() {
        let f = UserTagFilter::new(0x0f, 0x02);
        assert!(f.apply(read_ev(0xff)).is_some(), "read bypasses");
        assert!(
            f.apply(ev(0xff, EventMask::PROPERTY)).is_some(),
            "property bypasses"
        );
        // `first` is still armed, so this non-matching event still passes.
        assert!(f.apply(ev(0xff, EventMask::VALUE)).is_some(), "first event");
        assert!(
            f.apply(ev(0xff, EventMask::VALUE)).is_none(),
            "second is gated"
        );
    }

    /// The bare `{"utag":{}}` defaults: mask = 0xffffffff, value = 0, so
    /// only a zero user tag survives.
    #[test]
    fn default_mask_passes_only_a_zero_tag() {
        let f = UserTagFilter::new(UserTagFilter::DEFAULT_MASK, 0);
        assert!(f.apply(ev(7, EventMask::VALUE)).is_some(), "first event");
        assert!(f.apply(ev(7, EventMask::VALUE)).is_none());
        assert!(f.apply(ev(0, EventMask::VALUE)).is_some());
    }
}
