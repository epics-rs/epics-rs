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
pub mod sync;
pub mod ts;
pub mod utag;

pub use arr::{ArrayFilter, ArrayFilterConfig};
pub use dbnd::{DeadbandFilter, DeadbandMode};
pub use decimate::DecimateFilter;
pub use parser::{
    ChannelName, FilterParseError, ParsedChannelName, parse_channel_name, parse_filter_chain,
    split_channel_name, try_parse_filter_chain,
};
pub use sync::{DbState, DbStateRegistry, SyncFilter, SyncMode, db_state_registry};
pub use ts::TimestampFilter;
pub use utag::UserTagFilter;

/// One event passed through the filter chain. Wraps the standard
/// [`MonitorEvent`] with the C field-log context flag. The originating
/// [`EventMask`] filters consult for the "always-pass alarm / property"
/// rule (epics-base 446e0d4a) lives on the event itself
/// (`event.mask`) — one mask, one owner, so a filter that transforms
/// the event cannot leave a stale duplicate behind.
#[derive(Debug, Clone)]
pub struct FilteredMonitorEvent {
    pub event: MonitorEvent,
    /// `true` when this event originates from a single-read context
    /// (DB-link / `caget`), not a monitor stream. Mirrors C
    /// `db_field_log.ctx == dbfl_context_read`: `decimate.c:64` and
    /// `sync.c:98` both `return pfl` unchanged in that case so a
    /// one-shot read is never consumed by the decimator counter nor
    /// gated by the sync state machine.
    pub read_context: bool,
}

impl FilteredMonitorEvent {
    /// Construct a monitor-stream event (`read_context = false`).
    pub fn new(event: MonitorEvent) -> Self {
        Self {
            event,
            read_context: false,
        }
    }

    /// Construct a single-read-context event — `decimate` / `sync`
    /// pass these through unchanged (C `dbfl_context_read`).
    pub fn new_read(event: MonitorEvent) -> Self {
        Self {
            event,
            read_context: true,
        }
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

    /// Maximum number of array elements this filter emits for an
    /// `input`-element array (C `dbChannelFinalElements`). The default
    /// is the identity — only count-reshaping filters (`arr`) override
    /// it. Used to advertise the filter-adjusted element count on the
    /// CA CREATE_CHAN reply so clients request the sliced count rather
    /// than the unfiltered native count. Value-gating filters (`dbnd`,
    /// `dec`, `sync`) drop whole events but never change an event's
    /// element count, so they keep the identity.
    fn final_element_count(&self, input: usize) -> usize {
        input
    }
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

    /// Final element count this chain produces for a `native`-element
    /// input (C `dbChannelFinalElements`): fold each filter's
    /// per-element-count transform head-to-tail. An empty chain is the
    /// identity, so an unfiltered channel keeps its native count.
    pub fn final_element_count(&self, native: usize) -> usize {
        self.filters
            .iter()
            .fold(native, |n, f| f.final_element_count(n))
    }

    /// Iterate over the filters in registration order.
    pub fn iter(&self) -> impl Iterator<Item = &Arc<dyn SubscriptionFilter>> {
        self.filters.iter()
    }

    /// Apply the chain to a single value wrapped in a synthetic
    /// [`FilteredMonitorEvent`] with `EventMask::VALUE`, then return the
    /// (possibly transformed) value. `None` means a filter dropped the
    /// synthetic event. `read_context` selects the C field-log context:
    /// `true` => `dbfl_context_read` (one-shot DB read — `dec`/`sync`
    /// short-circuit), `false` => `dbfl_context_event` (monitor
    /// single-event post — `dec`/`sync` state machines run). This is the
    /// single owner shared by [`apply_to_read_value`](Self::apply_to_read_value)
    /// and [`apply_to_event_value`](Self::apply_to_event_value).
    fn apply_single_value(
        &self,
        value: crate::types::EpicsValue,
        read_context: bool,
    ) -> Option<crate::types::EpicsValue> {
        if self.filters.is_empty() {
            return Some(value);
        }
        use crate::server::pv::MonitorEvent;
        use crate::server::snapshot::Snapshot;
        let snap = Snapshot::new(value, 0, 0, std::time::SystemTime::now());
        let event = MonitorEvent {
            snapshot: std::sync::Arc::new(snap),
            origin: 0,
            mask: EventMask::VALUE,
        };
        let wrapped = if read_context {
            FilteredMonitorEvent::new_read(event)
        } else {
            FilteredMonitorEvent::new(event)
        };
        let filtered = self.apply(wrapped)?;
        // Sole owner on this path (the snapshot was built one statement above
        // and never shared), so `unwrap_or_clone` moves rather than copies.
        Some(std::sync::Arc::unwrap_or_clone(filtered.event.snapshot).value)
    }

    /// epics-base PR `17a8dbc` parity: apply the filter chain to a
    /// single one-shot read (DB link `dbDbGetValue` path / CA
    /// `READ`/`READ_NOTIFY`). Read context (C `dbfl_context_read`).
    ///
    /// Stream-only filters (`dbnd`, `dec`, `sync`) do NOT make sense
    /// in a single-read context — `dbnd` would always pass on the
    /// first call (no prior value), `dec` would emit every Nth call
    /// from a continuous counter, and `sync` would be governed by a
    /// state never observed in single-read flow. Only `arr` (array
    /// slicing) and `ts` (timestamp tagging) have meaningful single-
    /// read semantics. Operators are responsible for using filters
    /// that match their intent — the framework executes whatever
    /// chain is configured.
    pub fn apply_to_read_value(
        &self,
        value: crate::types::EpicsValue,
    ) -> Option<crate::types::EpicsValue> {
        self.apply_single_value(value, true)
    }

    /// epics-base parity for a CA monitor single-event post
    /// (`db_post_single_event`, `rsrv/camessage.c:1117-1122`,
    /// `1851-1853`): the initial monitor event and access-rights
    /// transition events are queued via `db_create_event_log`
    /// (`dbEvent.c:746-752`, `dbfl_context_event`) then run through
    /// `dbChannelRunPreChain`, and `db_queue_event_log` fires only when
    /// the filtered log is non-null (`dbEvent.c:922-924`).
    ///
    /// Event context (the opposite of [`apply_to_read_value`](Self::apply_to_read_value)):
    /// `dec`/`sync` state machines DO run, so an initial or
    /// access-transition post is decimated or gated exactly like a
    /// natural update. `None` means the chain dropped the post — the
    /// caller MUST send no frame (matching `if(pLog)` in C), never fall
    /// back to the unfiltered value.
    pub fn apply_to_event_value(
        &self,
        value: crate::types::EpicsValue,
    ) -> Option<crate::types::EpicsValue> {
        self.apply_single_value(value, false)
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
        let snapshot = std::sync::Arc::new(Snapshot::new(
            EpicsValue::Double(v),
            0,
            0,
            SystemTime::UNIX_EPOCH,
        ));
        FilteredMonitorEvent::new(MonitorEvent {
            snapshot,
            origin: 0,
            mask: EventMask::VALUE,
        })
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

    /// `final_element_count` folds each filter's count transform
    /// head-to-tail (C `dbChannelFinalElements`).
    #[test]
    fn final_element_count_folds_through_chain() {
        use super::parse_filter_chain;
        // Empty chain is the identity — an unfiltered channel keeps its
        // native count.
        assert_eq!(FilterChain::new().final_element_count(10), 10);
        // `arr` reshapes the count; a trailing value-gating filter
        // (`dbnd`) leaves it unchanged.
        let chain = parse_filter_chain(r#"{"arr":{"s":5,"e":7},"dbnd":{"d":0.5}}"#);
        assert_eq!(chain.len(), 2);
        assert_eq!(chain.final_element_count(10), 3);
        // A value-gating-only chain keeps the native count.
        let dbnd_only = parse_filter_chain(r#"{"dbnd":{"d":0.5}}"#);
        assert_eq!(dbnd_only.final_element_count(10), 10);
    }

    #[test]
    fn debug_renders_filter_names() {
        let mut chain = FilterChain::new();
        chain.push(Arc::new(Tally::new("dbnd")));
        chain.push(Arc::new(Tally::new("arr")));
        let s = format!("{:?}", chain);
        assert_eq!(s, r#"["dbnd", "arr"]"#);
    }

    // ---- monitor single-event post vs one-shot read context ----

    /// a `dec` filter that decimates the first slot
    /// (`offset = 1`, so window position 0 is dropped) suppresses the
    /// EVENT-context single-event post (C `db_post_single_event` runs
    /// the pre-chain in `dbfl_context_event`) but is bypassed by the
    /// READ context (C `dbfl_context_read`). Same chain, opposite
    /// outcome — proving `apply_to_event_value` uses event context.
    #[test]
    fn apply_to_event_value_decimates_while_read_value_bypasses() {
        use super::parse_filter_chain;
        let value = EpicsValue::Double(7.0);
        // Read context: `dec` short-circuits → value passes unchanged, and
        // consumes no slot, so it can run any number of times.
        let read_chain = parse_filter_chain(r#"{"dec":{"n":2}}"#);
        for _ in 0..3 {
            assert!(
                matches!(
                    read_chain.apply_to_read_value(value.clone()),
                    Some(EpicsValue::Double(v)) if v == 7.0
                ),
                "read context bypasses the decimator"
            );
        }
        // Event context: the counter runs, so the second post of each
        // 2-window is dropped.
        let event_chain = parse_filter_chain(r#"{"dec":{"n":2}}"#);
        assert!(
            event_chain.apply_to_event_value(value.clone()).is_some(),
            "first single-event post is the head of the window"
        );
        assert!(
            event_chain.apply_to_event_value(value).is_none(),
            "event context decimates the second single-event post"
        );
    }

    /// Doc test #1: a `sync` filter gating `while` a cleared
    /// named state drops every event in EVENT context (`actstate ==
    /// false`), so the initial monitor snapshot is suppressed — while
    /// the same chain passes a one-shot READ unchanged.
    #[test]
    fn apply_to_event_value_suppresses_sync_while_read_value_passes() {
        use super::parse_filter_chain;
        let value = EpicsValue::Double(1.0);
        // `sync.c`'s `parse_ok` resolves the state with `dbStateFind`, so it
        // has to exist before the channel names it.
        super::db_state_registry().get_or_create("BFR7:GATE");
        let read_chain = parse_filter_chain(r#"{"sync":{"while":"BFR7:GATE"}}"#);
        assert!(
            read_chain.apply_to_read_value(value.clone()).is_some(),
            "read context bypasses the sync gate"
        );
        let event_chain = parse_filter_chain(r#"{"sync":{"while":"BFR7:GATE"}}"#);
        assert!(
            event_chain.apply_to_event_value(value).is_none(),
            "event context gates the initial post on a cleared state"
        );
    }

    /// Doc test #2: when a filter drops the single-event post,
    /// `apply_to_event_value` returns `None` — there is NO fallback to
    /// the unfiltered value (the CA call sites translate `None` into
    /// "send no frame").
    #[test]
    fn apply_to_event_value_drop_yields_none_no_fallback() {
        let mut chain = FilterChain::new();
        chain.push(Arc::new(DropAll));
        assert!(
            chain
                .apply_to_event_value(EpicsValue::Double(42.0))
                .is_none(),
            "a dropped post yields None, not the unfiltered value"
        );
    }

    /// an empty chain is the identity in BOTH contexts — the
    /// initial post is sent with the unmodified value (matching a CA
    /// channel that carried no `.{...}` filter suffix).
    #[test]
    fn apply_to_event_and_read_value_empty_chain_identity() {
        let chain = FilterChain::new();
        assert!(matches!(
            chain.apply_to_event_value(EpicsValue::Double(3.0)),
            Some(EpicsValue::Double(v)) if v == 3.0
        ));
        assert!(matches!(
            chain.apply_to_read_value(EpicsValue::Double(3.0)),
            Some(EpicsValue::Double(v)) if v == 3.0
        ));
    }

    /// Doc test #5: `arr` slicing is NOT context-gated, so the
    /// EVENT-context initial post still applies the array transform
    /// (only `dec`/`sync` distinguish read vs event context).
    #[test]
    fn apply_to_event_value_arr_slice_still_applies() {
        use super::parse_filter_chain;
        let chain = parse_filter_chain(r#"{"arr":{"s":1,"e":2}}"#);
        let out = chain
            .apply_to_event_value(EpicsValue::DoubleArray(vec![10.0, 20.0, 30.0, 40.0]))
            .expect("arr passes the event through");
        assert!(
            matches!(out, EpicsValue::DoubleArray(v) if v == vec![20.0, 30.0]),
            "arr slices [s=1,e=2] in event context too"
        );
    }
}
