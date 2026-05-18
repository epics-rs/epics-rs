//! [`LinkSet`] — pluggable backend for `pva://` / `ca://` link
//! resolution.
//!
//! Mirrors the C EPICS `lset` (link set) abstraction used by libdbCore
//! to delegate link operations to a pluggable backend. We expose a
//! pure-Rust trait so the bridge crate can wire up `pvalink` /
//! `calink` without epics-base-rs having to know about either
//! protocol.
//!
//! At runtime [`super::PvDatabase`] holds a registry keyed by URL
//! scheme (`"pva"`, `"ca"`); each entry is an `Arc<dyn LinkSet>`.
//! Record-link reads dispatch through the matching lset before
//! falling back to the legacy `ExternalPvResolver` closure.
//!
//! The trait is **synchronous** — record processing is fundamentally
//! sync at the lset boundary in C EPICS, and most lset
//! implementations (pvalink, calink) maintain a cached snapshot
//! that satisfies sync reads without blocking. Implementations that
//! need to do async I/O can keep a `tokio::runtime::Handle` and
//! `block_on` internally.
//!
//! # Adding a new lset
//!
//! ```ignore
//! struct MyLset { /* ... */ }
//! impl LinkSet for MyLset {
//!     fn is_connected(&self, name: &str) -> bool { /* ... */ }
//!     fn get_value(&self, name: &str) -> Option<EpicsValue> { /* ... */ }
//!     /* etc. */
//! }
//! db.register_link_set("pva", Arc::new(MyLset { ... })).await;
//! ```

use std::sync::Arc;

use crate::types::EpicsValue;

/// Pluggable backend for one URL scheme's link operations.
///
/// All methods take `&self` so the implementation must use interior
/// mutability for any cached state. None / false is the
/// "unavailable" sentinel — the database falls back to a generic
/// LINK/INVALID alarm when an lset returns None.
pub trait LinkSet: Send + Sync {
    /// True iff a fresh value is available for `name` without
    /// blocking. Used by the record processing loop to decide
    /// whether to mark the record's STAT as LINK_ALARM.
    fn is_connected(&self, name: &str) -> bool;

    /// Read the current value of `name`. Returns None when the
    /// upstream isn't yet connected or the lset has no cache for
    /// this name.
    fn get_value(&self, name: &str) -> Option<EpicsValue>;

    /// Write `value` to `name`. Returns Err with a human-readable
    /// reason on failure (denied, type-mismatch, no-such-pv, etc.).
    /// Default impl rejects all writes — read-only lsets keep the
    /// default.
    fn put_value(&self, name: &str, value: EpicsValue) -> Result<(), String> {
        let _ = (name, value);
        Err("link set is read-only".into())
    }

    /// Most recent alarm message string from the upstream PV, when
    /// available. None means no alarm or no cache.
    fn alarm_message(&self, _name: &str) -> Option<String> {
        None
    }

    /// Alarm severity (`0 = NO_ALARM` … `3 = INVALID`) to fold into
    /// the owning record's `LINK_ALARM`, when the link should
    /// propagate one.
    ///
    /// `None` means "do not propagate" — either the upstream has no
    /// alarm, the lset has no cache, or the link's maximize-severity
    /// mode (`NMS`/`MS`/`MSI`) suppresses it. The lset is expected to
    /// apply that mode gate itself (the `pva://X?sevr=MS` modifier is
    /// stripped before epics-base-rs sees the link, so only the lset
    /// retains it). A returned `Some(sev)` is therefore already
    /// gated and the record processing loop propagates it verbatim
    /// as a maximize-severity contribution. Mirrors pvxs
    /// `pvalink_lset.cpp` `pvaGetAlarm` feeding `recGblSetSevr`.
    fn alarm_severity(&self, _name: &str) -> Option<i32> {
        None
    }

    /// `(seconds_past_epoch, nanoseconds)` from the upstream PV's
    /// timestamp slot, when available.
    fn time_stamp(&self, _name: &str) -> Option<(i64, i32)> {
        None
    }

    /// Enumerate every PV name this lset has *opened* (i.e., is
    /// actively tracking). Used by `dbpvxr` to dump per-record
    /// link state without forcing the caller to know the full
    /// name list up-front.
    fn link_names(&self) -> Vec<String> {
        Vec::new()
    }
}

/// Type-erased lset reference held by the [`LinkSetRegistry`].
pub type DynLinkSet = Arc<dyn LinkSet>;

/// Per-scheme registry. Wrapped in [`tokio::sync::RwLock`] inside
/// [`super::PvDatabase`] so registration and read-paths are
/// independently mutable.
#[derive(Default)]
pub struct LinkSetRegistry {
    inner: std::collections::HashMap<String, DynLinkSet>,
}

impl LinkSetRegistry {
    pub fn new() -> Self {
        Self {
            inner: std::collections::HashMap::new(),
        }
    }

    /// Register `lset` under `scheme`. Subsequent calls for the same
    /// scheme replace the previous binding.
    pub fn register(&mut self, scheme: &str, lset: DynLinkSet) {
        self.inner.insert(scheme.to_string(), lset);
    }

    /// Look up the lset for `scheme`. Returns `None` when nothing is
    /// registered under that scheme.
    pub fn get(&self, scheme: &str) -> Option<DynLinkSet> {
        self.inner.get(scheme).cloned()
    }

    /// Names of every registered scheme (`["pva", "ca", ...]`).
    pub fn schemes(&self) -> Vec<String> {
        self.inner.keys().cloned().collect()
    }

    /// Number of registered schemes.
    pub fn len(&self) -> usize {
        self.inner.len()
    }

    pub fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct StubLset;
    impl LinkSet for StubLset {
        fn is_connected(&self, _: &str) -> bool {
            true
        }
        fn get_value(&self, _: &str) -> Option<EpicsValue> {
            Some(EpicsValue::Long(42))
        }
    }

    #[test]
    fn register_and_lookup() {
        let mut reg = LinkSetRegistry::new();
        assert!(reg.is_empty());
        reg.register("pva", Arc::new(StubLset));
        assert_eq!(reg.len(), 1);
        let lset = reg.get("pva").expect("registered");
        assert!(lset.is_connected("anything"));
        assert_eq!(lset.get_value("anything"), Some(EpicsValue::Long(42)));
    }

    #[test]
    fn unknown_scheme_returns_none() {
        let reg = LinkSetRegistry::new();
        assert!(reg.get("missing").is_none());
    }
}
