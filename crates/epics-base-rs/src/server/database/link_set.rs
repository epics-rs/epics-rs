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

/// DBF field type a link's value maps to — the Rust counterpart of
/// the C `DBF_*` codes pvxs `pvaGetDBFtype` returns.
///
/// Mirrors `pvxs/ioc/pvalink_lset.cpp:199` (`pvaGetDBFtype`), which
/// maps the cached NT value's `TypeCode` to a `DBF_*` constant; an
/// NT `enum_t` structure maps to `DBF_ENUM`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LinkDbfType {
    Char,
    UChar,
    Short,
    UShort,
    Long,
    ULong,
    Int64,
    UInt64,
    Float,
    Double,
    String,
    Enum,
}

/// Remote display / control / valueAlarm metadata snapshot for a
/// link, as exposed by pvxs's pvalink lset metadata getters.
///
/// Mirrors the pvxs `pvalink_lset.cpp` metadata getter set installed
/// at `pvxs/ioc/pvalink_lset.cpp:700`:
/// `pvaGetDBFtype`, `pvaGetElements`, `pvaGetControlLimits`,
/// `pvaGetGraphicLimits`, `pvaGetAlarmLimits`, `pvaGetPrecision`,
/// `pvaGetUnits`.
///
/// Every field is optional: pvxs's getters read the cached NT
/// structure with `Value::as`, which leaves the caller's buffer
/// unchanged when the sub-field is absent. `None` here means the
/// remote NT value carried no such metadata — the record support
/// then keeps its local/default metadata, exactly as the C path does.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct LinkMetadata {
    /// DBF type the remote value maps to (`pvaGetDBFtype`). A connected
    /// link always reports a type — an unmappable value shape falls back
    /// to `Long`, the `default:` arm of pvxs `pvaGetDBFtype`
    /// (`pvalink_lset.cpp:199-236`). `None` therefore means "not
    /// connected" (no cached value), never "connected but unmappable".
    pub dbf_type: Option<LinkDbfType>,
    /// Element count: array length, or `1` for a scalar / any connected
    /// non-array shape (`pvaGetElements`, `pvalink_lset.cpp:242-254`).
    /// As with `dbf_type`, `None` means "not connected".
    pub element_count: Option<i64>,
    /// `display.limitLow` / `display.limitHigh` (`pvaGetGraphicLimits`).
    pub graphic_limits: Option<(f64, f64)>,
    /// `control.limitLow` / `control.limitHigh` (`pvaGetControlLimits`).
    pub control_limits: Option<(f64, f64)>,
    /// `valueAlarm.{lowAlarmLimit,lowWarningLimit,highWarningLimit,
    /// highAlarmLimit}` as `(lolo, lo, hi, hihi)` (`pvaGetAlarmLimits`).
    pub alarm_limits: Option<(f64, f64, f64, f64)>,
    /// `display.precision` (`pvaGetPrecision`).
    pub precision: Option<i16>,
    /// `display.units` (`pvaGetUnits`).
    pub units: Option<String>,
    /// `display.description` — carried so a link snapshot is complete;
    /// pvxs exposes it through the same `fld_meta` cache.
    pub description: Option<String>,
}

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

    /// Flush any OUT-link writes the lset has queued but not yet sent —
    /// the production drain trigger for an async OUT channel owner.
    ///
    /// Two queued states this drains: a write deferred for sibling
    /// coalescing, and a write that failed mid-disconnect and is held
    /// for replay once the upstream reconnects (`retry`). The database
    /// calls this after every external OUT-link write so the
    /// "retry on connect" path has a production caller from record
    /// processing — not only test code. Default no-op: a synchronous
    /// lset (DB links, a read-only lset) queues nothing.
    ///
    /// Mirrors the role of pvxs's shared `pvaLinkChannel::put()` being
    /// driven from record processing rather than left to manual calls
    /// (`pvxs/ioc/pvalink_lset.cpp:647`, `pvalink_channel.cpp:220-263`).
    fn flush_puts(&self) {}

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

    /// Remote alarm *status* code (the EPICS `alarm_status` enum:
    /// `0 = NO_ALARM`, `1 = READ`, … `17 = COMM`, …) from the upstream
    /// PV, when available.
    ///
    /// used to honour the `MSS` (maximize-severity-and-
    /// status) link modifier — the owning record then adopts the remote
    /// STAT instead of the generic `LINK_ALARM`. `None` means the lset
    /// cannot report a remote status (no cache, or the link set does not
    /// track it); the caller falls back to `LINK_ALARM`, which is the
    /// behaviour for every non-`MSS` modifier and for lsets that leave
    /// this default. Mirrors pvxs `pvalink_lset.cpp` `pvaGetAlarm`
    /// surfacing the remote `alarm.status` to `recGblSetSevrMsg`.
    fn alarm_status(&self, _name: &str) -> Option<i32> {
        None
    }

    /// `(seconds_past_epoch, nanoseconds, userTag)` from the upstream
    /// PV's timestamp slot, when available. The `userTag` is the remote
    /// `timeStamp.userTag` widened to the 64-bit `epicsUTag` tag without
    /// sign extension, or `0` when the source carries none (CA links, or
    /// a PVA source whose timeStamp omits the field).
    fn time_stamp(&self, _name: &str) -> Option<(i64, i32, u64)> {
        None
    }

    /// Remote display / control / valueAlarm metadata for `name`, as
    /// a single snapshot.
    ///
    /// The Rust counterpart of pvxs's pvalink lset metadata getter
    /// set (`pvaGetDBFtype`, `pvaGetElements`, `pvaGetControlLimits`,
    /// `pvaGetGraphicLimits`, `pvaGetAlarmLimits`, `pvaGetPrecision`,
    /// `pvaGetUnits` — installed at `pvxs/ioc/pvalink_lset.cpp:700`).
    /// A structured snapshot is used instead of seven separate trait
    /// methods so the lset reads its cache once and record support
    /// gets every linked-metadata field together.
    ///
    /// `None` means the lset has no cached value for `name` (not yet
    /// connected); a `Some(LinkMetadata)` with individual `None`
    /// fields means the remote NT value simply did not carry that
    /// piece of metadata — the record then keeps its local default,
    /// matching the C getters that leave the caller's buffer
    /// untouched on a missing sub-field. Default impl: no metadata.
    fn link_metadata(&self, _name: &str) -> Option<LinkMetadata> {
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
