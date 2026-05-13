//! Server-side channel-filter framework (epics-base 3.15.7).
//!
//! A *channel filter* sits between record processing and a subscriber's
//! transmit queue. Each emitted [`MonitorEvent`] flows through the
//! filter chain attached to the subscriber: any filter may transform
//! the event (e.g. array-slice via `arr`) or drop it (e.g. deadband
//! suppresses sub-threshold value changes).
//!
//! Filter chain semantics:
//!
//! 1. Filters run in registration order, head-to-tail.
//! 2. A filter returning `None` short-circuits the chain — the event
//!    is dropped, downstream filters are not consulted.
//! 3. A filter returning `Some(event)` may have mutated the event;
//!    the next filter sees the mutated version.
//!
//! Per the upstream `dbnd` design note (epics-base 446e0d4a) value
//! filters MUST always pass `DBE_ALARM` and `DBE_PROPERTY` events
//! through unchanged. The event mask travels alongside the snapshot
//! on [`FilteredMonitorEvent`] so filters can short-circuit on
//! alarm / property events without needing to inspect the snapshot's
//! alarm / metadata fields.
//!
//! This module ships the framework + the `dbnd` filter. `ts`, `arr`,
//! `decimate`, and `sync` land in follow-up commits.

use std::sync::Arc;

use crate::server::pv::MonitorEvent;
use crate::server::recgbl::EventMask;

pub mod arr;
pub mod dbnd;
pub mod decimate;
pub mod parser;
pub mod ts;

pub use arr::{ArrayFilter, ArrayFilterConfig};
pub use dbnd::{DeadbandFilter, DeadbandMode};
pub use decimate::DecimateFilter;
pub use parser::{ParsedChannelName, parse_filter_chain, split_channel_name};
pub use ts::TimestampFilter;

/// One event passed through the filter chain. Wraps the standard
/// [`MonitorEvent`] with the originating [`EventMask`] so filters
/// can implement the "always-pass alarm / property" rule from
/// epics-base 446e0d4a without poking at the snapshot internals.
#[derive(Debug, Clone)]
pub struct FilteredMonitorEvent {
    pub event: MonitorEvent,
    pub mask: EventMask,
}

impl FilteredMonitorEvent {
    pub fn new(event: MonitorEvent, mask: EventMask) -> Self {
        Self { event, mask }
    }
}

/// A pluggable channel filter. Implementors must be `Send + Sync` so
/// the same `Arc<dyn SubscriptionFilter>` can sit in any subscriber's
/// chain across the async runtime.
///
/// Filter state (last-sent value for `dbnd`, counter for `decimate`)
/// lives inside the implementation via interior mutability —
/// `apply` takes `&self`. This keeps the trait object usable from
/// the synchronous fan-out path in `RecordInstance::notify_field_*`.
pub trait SubscriptionFilter: Send + Sync + 'static {
    /// Short name used by the PV-name JSON parser and by debug
    /// rendering (e.g. `"dbnd"`, `"arr"`, `"ts"`).
    fn name(&self) -> &'static str;

    /// Apply the filter to one outgoing event. Return `Some(event)`
    /// to pass through (possibly modified) or `None` to drop.
    fn apply(&self, event: FilteredMonitorEvent) -> Option<FilteredMonitorEvent>;
}

/// Ordered chain of filters owned by one subscriber. The default is
/// an empty chain — every event passes unchanged.
#[derive(Default, Clone)]
pub struct FilterChain {
    filters: Vec<Arc<dyn SubscriptionFilter>>,
}

impl FilterChain {
    pub fn new() -> Self {
        Self::default()
    }

    /// Append a filter to the chain. Order matters — the head of
    /// the chain sees the event first, so a `decimate` ahead of a
    /// `dbnd` decimates first and then drops sub-threshold changes
    /// out of the surviving fraction, while the reverse order
    /// decimates after the deadband has already thinned the stream.
    pub fn push(&mut self, filter: Arc<dyn SubscriptionFilter>) {
        self.filters.push(filter);
    }

    /// `true` iff the chain has at least one filter.
    pub fn is_empty(&self) -> bool {
        self.filters.is_empty()
    }

    /// Apply all filters head-to-tail. The first `None` short-
    /// circuits and the chain returns `None`. Returns the (possibly
    /// transformed) event when every filter passes.
    pub fn apply(&self, event: FilteredMonitorEvent) -> Option<FilteredMonitorEvent> {
        let mut current = event;
        for f in &self.filters {
            current = f.apply(current)?;
        }
        Some(current)
    }

    /// Number of filters in the chain (mostly for tests / debug).
    pub fn len(&self) -> usize {
        self.filters.len()
    }

    /// Iterate over the filters in registration order.
    pub fn iter(&self) -> impl Iterator<Item = &Arc<dyn SubscriptionFilter>> {
        self.filters.iter()
    }
}

impl std::fmt::Debug for FilterChain {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_list()
            .entries(self.filters.iter().map(|filt| filt.name()))
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::snapshot::Snapshot;
    use crate::types::EpicsValue;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::time::SystemTime;

    /// Test-only filter that counts invocations and unconditionally
    /// passes the event through. Lets the chain tests verify
    /// invocation order and short-circuit behaviour without depending
    /// on the real `dbnd` implementation.
    struct Tally {
        name: &'static str,
        count: AtomicU32,
    }

    impl Tally {
        fn new(name: &'static str) -> Self {
            Self {
                name,
                count: AtomicU32::new(0),
            }
        }
    }

    impl SubscriptionFilter for Tally {
        fn name(&self) -> &'static str {
            self.name
        }
        fn apply(&self, event: FilteredMonitorEvent) -> Option<FilteredMonitorEvent> {
            self.count.fetch_add(1, Ordering::Relaxed);
            Some(event)
        }
    }

    /// Test-only filter that always drops.
    struct DropAll;
    impl SubscriptionFilter for DropAll {
        fn name(&self) -> &'static str {
            "drop"
        }
        fn apply(&self, _e: FilteredMonitorEvent) -> Option<FilteredMonitorEvent> {
            None
        }
    }

    fn make_event(v: f64) -> FilteredMonitorEvent {
        let snapshot = Snapshot::new(EpicsValue::Double(v), 0, 0, SystemTime::UNIX_EPOCH);
        FilteredMonitorEvent::new(
            MonitorEvent {
                snapshot,
                origin: 0,
            },
            EventMask::VALUE,
        )
    }

    /// Empty chain is the identity — every event passes unchanged.
    #[test]
    fn empty_chain_passes_through() {
        let chain = FilterChain::new();
        let out = chain.apply(make_event(1.0)).expect("empty chain passes");
        assert!(matches!(out.event.snapshot.value, EpicsValue::Double(v) if v == 1.0));
    }

    /// Each filter sees the event in registration order.
    #[test]
    fn chain_invokes_filters_in_order() {
        let a = Arc::new(Tally::new("a"));
        let b = Arc::new(Tally::new("b"));
        let mut chain = FilterChain::new();
        chain.push(a.clone());
        chain.push(b.clone());
        for _ in 0..3 {
            assert!(chain.apply(make_event(1.0)).is_some());
        }
        assert_eq!(a.count.load(Ordering::Relaxed), 3);
        assert_eq!(b.count.load(Ordering::Relaxed), 3);
    }

    /// A `None` from one filter short-circuits the rest of the chain.
    #[test]
    fn drop_short_circuits_downstream() {
        let a = Arc::new(Tally::new("a"));
        let b = Arc::new(Tally::new("b"));
        let mut chain = FilterChain::new();
        chain.push(a.clone());
        chain.push(Arc::new(DropAll));
        chain.push(b.clone());
        assert!(chain.apply(make_event(1.0)).is_none());
        assert_eq!(a.count.load(Ordering::Relaxed), 1, "a runs before the drop");
        assert_eq!(
            b.count.load(Ordering::Relaxed),
            0,
            "b is short-circuited by the upstream drop"
        );
    }

    #[test]
    fn debug_renders_filter_names() {
        let mut chain = FilterChain::new();
        chain.push(Arc::new(Tally::new("dbnd")));
        chain.push(Arc::new(Tally::new("arr")));
        let s = format!("{:?}", chain);
        assert_eq!(s, r#"["dbnd", "arr"]"#);
    }
}
