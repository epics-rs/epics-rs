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
//! The trait is **split by thread**, mirroring the C `dbCa` split
//! between the record-processing thread and the `dbCaTask`
//! (`dbCa.c:1191-1248`):
//!
//! * **Synchronous methods** are the ones record processing calls while
//!   it holds the record's advisory write gate (C `dbScanLock`). They
//!   answer from cached, monitor-fed state and MUST NOT perform I/O —
//!   C `dbCaGetLink` copies out of `pca->pgetNative` and never touches
//!   the wire (`dbCa.c:448-535`). Their signature is what enforces
//!   that: a `fn` cannot await, so it cannot suspend the record thread.
//! * **Async methods** ([`LinkSet::get_value`],
//!   [`LinkSet::connect_link`], [`LinkSet::put_value`],
//!   [`LinkSet::flush_puts`]) are the `dbCaTask` half. They run on the
//!   database's link work owner, never on a record-processing thread,
//!   and MAY block on the network.
//!
//! Before this split the "MUST NOT perform I/O" rule was a doc comment
//! on an `async fn`, so nothing stopped a new lset from suspending
//! record processing inside the gate. It is now a type-level property.
//!
//! # Adding a new lset
//!
//! ```ignore
//! struct MyLset { /* ... */ }
//! #[epics_base_rs::async_trait]
//! impl LinkSet for MyLset {
//!     fn is_connected(&self, name: &str) -> bool { /* cached */ }
//!     fn get_cached_value(&self, name: &str) -> Option<EpicsValue> { /* cached */ }
//!     async fn get_value(&self, name: &str) -> Option<EpicsValue> { /* may do I/O */ }
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

/// How an external OUT-link write should be delivered to the lset.
///
/// Mirrors the C dbCore split between a plain link put and a
/// put-notify-aware put: `dbPutLink` (synchronous, no completion
/// callback) vs `dbPutLinkAsync` (issued from `dbNotify`, where the
/// source record's processing is held until the downstream put
/// completes). pvxs's pvalink lset realises the same split as
/// `pvaPutValue` (plain, `wait=false`) vs `pvaPutValueAsync`
/// (`wait=true`, which sets `record._options.block` so the PUT
/// request carries the block option and the source record is parked
/// in `after_put` until the server acknowledges completion) —
/// `pvxs/ioc/pvalink_lset.cpp` `putValue` / `putValueAsync`.
///
/// The database selects the op from the write context: a write that
/// originates inside a put-notify / blocking-put chain (the source
/// record carries a completion wait-set) uses [`Async`]; a plain
/// record-processing OUT write uses [`Plain`].
///
/// [`Async`]: LinkPutOp::Async
/// [`Plain`]: LinkPutOp::Plain
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum LinkPutOp {
    /// Plain put — fire-and-forget from the lset's perspective. Maps to
    /// pvxs `pvaPutValue` (`wait=false`) / C `dbPutLink`.
    #[default]
    Plain,
    /// Completion-aware put — the originating record is part of a
    /// put-notify / blocking-put chain. Maps to pvxs `pvaPutValueAsync`
    /// (`wait=true`, `record._options.block`) / C `dbPutLinkAsync`.
    Async,
}

/// Whether an external OUT link will accept a staged write right now —
/// the answer to C `dbCaPutLinkCallback`'s first gate:
///
/// ```c
/// if (!pca->isConnected || !pca->hasWriteAccess) {
///     epicsMutexUnlock(pca->lock);
///     return -1;
/// }
/// ```
/// (`dbCa.c:558-561`).
///
/// Answered from cached state only: it runs on a record-processing thread
/// inside the record's advisory write gate, which is exactly where C never
/// touches the network.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PutAdmission {
    /// The lset tracks this link and its channel is up — stage the write.
    /// C's fall-through to `addAction` (`dbCa.c:622`).
    Connected,
    /// The lset tracks this link and the channel is DOWN. C refuses the put
    /// with `-1` and stages nothing (`dbCa.c:558-561`); the caller raises the
    /// owning record's LINK/INVALID through `dbPutLink`'s `setLinkAlarm`
    /// (`dbLink.c:434-448`).
    Disconnected,
    /// The lset has never opened this link, so it cannot answer. C cannot
    /// reach this state: `dbCaAddLink` opens every CA link at record-init
    /// time (`dbCa.c` `addAction(pca, CA_CONNECT)`), so by the first
    /// `dbCaPutLink` the `caLink` always exists. Our lsets open lazily on
    /// first use instead, so the write is staged and the lset's own
    /// `put_value` performs the open — dropping it would mean an OUT link
    /// that never opens and therefore never connects.
    Unopened,
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
    /// ENUM state labels of the remote channel, when it has any. The label
    /// table a `DBR_STRING`-requesting reader (stringin/lsi INP, printf
    /// `%s`) renders a remote enum index through — C `dbCa` keeps a second
    /// `DBR_STRING` monitor (`pgetString`) for that read; here the labels
    /// ride the same attribute fetch as the limits (CA: `DBR_CTRL_ENUM`).
    pub enum_choices: Option<Vec<String>>,
}

/// Ungated remote alarm snapshot for a link — the remote
/// `(severity, status, message)` the upstream PV carried at the last
/// successful value read, WITHOUT the maximize-severity
/// (`MS`/`NMS`/`MSI`) gate that [`LinkSet::alarm_severity`] applies for
/// owning-record propagation.
///
/// This is the DB-link inspection counterpart pvxs exposes through
/// `dbGetAlarm` / `dbGetAlarmMsg` — `pvaGetAlarmMsg` returns the cached
/// `snap_severity` / `snap_message` directly and never consults the
/// link's `sevr` mode (`pvxs/ioc/pvalink_lset.cpp:542-575`). A default
/// `NMS` link must still report its remote severity here even though it
/// does not maximize the owning record's severity.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct RemoteAlarm {
    /// Remote alarm severity (`0 = NO_ALARM` … `3 = INVALID`), the raw
    /// cached `alarm.severity` — never gated by the link's `sevr` mode.
    pub severity: i32,
    /// Remote alarm status code, derived from `severity` exactly as
    /// pvxs `pvaGetAlarmMsg` does (`LINK_ALARM` when severity is
    /// non-`NO_ALARM`, else `NO_ALARM` — `pvalink_lset.cpp:551`). See
    /// [`RemoteAlarm::from_severity_message`].
    pub status: i32,
    /// Remote `alarm.message`. Empty when the remote carried none or
    /// the severity is `NO_ALARM` (pvxs clears `snap_message` unless
    /// `snap_severity != 0` — `pvalink_lset.cpp:418-422`).
    pub message: String,
}

impl RemoteAlarm {
    /// Build a snapshot whose `status` is derived from `severity`
    /// exactly as pvxs `pvaGetAlarmMsg` (`pvalink_lset.cpp:551`):
    /// `LINK_ALARM` when the remote severity is non-`NO_ALARM`, else
    /// `NO_ALARM`. status and severity cannot disagree by construction.
    pub fn from_severity_message(severity: i32, message: String) -> Self {
        let status = if severity != 0 {
            crate::server::recgbl::alarm_status::LINK_ALARM as i32
        } else {
            crate::server::recgbl::alarm_status::NO_ALARM as i32
        };
        Self {
            severity,
            status,
            message,
        }
    }
}

/// Pluggable backend for one URL scheme's link operations.
///
/// All methods take `&self` so the implementation must use interior
/// mutability for any cached state. None / false is the
/// "unavailable" sentinel — the database falls back to a generic
/// LINK/INVALID alarm when an lset returns None.
#[async_trait::async_trait]
pub trait LinkSet: Send + Sync {
    /// True iff a fresh value is available for `name` without
    /// blocking. Used by the record processing loop to decide
    /// whether to mark the record's STAT as LINK_ALARM.
    ///
    /// Synchronous: asked on the record-processing thread inside the
    /// record's advisory write gate. MUST NOT perform I/O.
    fn is_connected(&self, name: &str) -> bool;

    /// Read the current value of `name`. Returns None when the
    /// upstream isn't yet connected or the lset has no cache for
    /// this name.
    ///
    /// MAY perform I/O (open the channel, issue a one-shot GET). It is
    /// therefore called only from the database's link work owner task —
    /// the record-processing path uses [`Self::get_cached_value`].
    async fn get_value(&self, name: &str) -> Option<EpicsValue>;

    /// Read `name` from cached, monitor-fed state ONLY — the
    /// record-processing read. C `dbCaGetLink` (`dbCa.c:448-535`) copies
    /// out of `pca->pgetNative`, the buffer the CA monitor callback
    /// (`eventCallback`, `dbCa.c:925`) keeps fresh on the `dbCaTask`; it
    /// never opens a channel and never waits on the wire. Returns None
    /// when the link has no cached value yet, which is C returning -1 for
    /// `!pca->isConnected` (`dbCa.c:459-464`) — the reading record takes
    /// LINK/INVALID for that cycle.
    ///
    /// MUST NOT perform I/O — which is why this is a `fn` and
    /// [`Self::get_value`] is an `async fn`.
    ///
    /// Default: `None`, i.e. "this lset keeps no cache". That is C's
    /// `!pca->isConnected` arm verbatim: the reading record takes
    /// LINK/INVALID for the cycle and the database stages the link's
    /// open on the link work owner ([`Self::connect_link`]), which is
    /// what warms the cache for the next cycle. An lset that CAN answer
    /// from memory MUST override, or its links never read.
    fn get_cached_value(&self, name: &str) -> Option<EpicsValue> {
        let _ = name;
        None
    }

    /// Open (subscribe / connect) `name` so later
    /// [`Self::get_cached_value`] reads have a cache to serve — C
    /// `dbCaAddLink` (`dbCa.c:735-800`), which stages a `CA_CONNECT`
    /// action whose `ca_create_channel` + `ca_add_array_event` run on the
    /// `dbCaTask`, not on the caller.
    ///
    /// **Called from the database's link work owner task**, so it MAY
    /// block on the network. Idempotent: the owner may call it again for
    /// a link that is already open or still connecting.
    ///
    /// Default: drive the lset's own lazy open by reading through
    /// [`Self::get_value`] and discarding the result — correct for every
    /// existing lset, and it runs off the record-processing thread.
    async fn connect_link(&self, name: &str) {
        let _ = self.get_value(name).await;
    }

    /// Non-blocking admission gate for an OUT-link write, asked on the
    /// record-processing thread *before* the write is staged onto the
    /// database's link-put queue — C `dbCaPutLinkCallback`'s
    /// `if (!pca->isConnected || !pca->hasWriteAccess) return -1;`
    /// (`dbCa.c:558-561`).
    ///
    /// MUST NOT perform I/O. It is the one lset call left inside the
    /// record's advisory write gate, and the whole point of the queue is
    /// that nothing there touches the network.
    ///
    /// Default: derive from [`Self::is_connected`], which the trait already
    /// documents as answerable "without blocking". An lset whose OUT links
    /// live in a different cache than its INP links (pvalink keys its
    /// registry on direction) MUST override, or every OUT write to a
    /// perfectly healthy channel is refused.
    fn put_admission(&self, name: &str) -> PutAdmission {
        if self.is_connected(name) {
            PutAdmission::Connected
        } else {
            PutAdmission::Disconnected
        }
    }

    /// Write `value` to `name` with the delivery semantics named by
    /// `op` ([`LinkPutOp::Plain`] for a fire-and-forget put,
    /// [`LinkPutOp::Async`] for a put that is part of a put-notify /
    /// blocking-put chain). Returns Err with a human-readable reason
    /// on failure (denied, type-mismatch, no-such-pv, etc.). Default
    /// impl rejects all writes — read-only lsets keep the default.
    ///
    /// **Called from the database's link-put owner task, never from a
    /// record-processing thread** — this is the `dbCaTask` half of the
    /// split (`dbCa.c:1226-1248`), so it may block on the network.
    async fn put_value(&self, name: &str, value: EpicsValue, op: LinkPutOp) -> Result<(), String> {
        let _ = (name, value, op);
        Err("link set is read-only".into())
    }

    /// Fire `name`'s forward link (FLNK): trigger the remote target to
    /// process, transferring no value.
    ///
    /// The lset counterpart of C `dbScanFwdLink` → `lset->scanForward`
    /// (`dbLink.c:475`), realised by the pvalink lset as `pvaScanForward`
    /// (`pvxs/ioc/pvalink_lset.cpp:672-688`). A forward link is never
    /// deferred ("FWD_LINK is never deferred, and always results in a
    /// Put") and carries no staged value: it forces the remote record to
    /// process when the source record fires its FLNK.
    ///
    /// The lset applies the same non-retry validity gate pvxs does
    /// (`pvalink_lset.cpp:677`): on a non-retry link that is not currently
    /// connected it performs NO trigger and returns `Err`, so the caller
    /// raises LINK/INVALID on the owning record — pvxs calls
    /// `recGblSetSevrMsg(LINK_ALARM, INVALID_ALARM, "Disconn")` there.
    ///
    /// Default impl: `Ok(())` no-op. A read-only or DB-local lset
    /// forwards nothing through this hook — a DB FLNK target is processed
    /// directly by the database's `scanOnce` path (the DB lset's
    /// `scanForward`), not through an external link set.
    fn scan_forward(&self, name: &str) -> Result<(), String> {
        let _ = name;
        Ok(())
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
    async fn flush_puts(&self) {}

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

    /// Ungated remote alarm snapshot — the remote `(severity, status,
    /// message)` after a successful value read, WITHOUT the
    /// maximize-severity (`MS`/`NMS`/`MSI`) gate that
    /// [`LinkSet::alarm_severity`] applies.
    ///
    /// This is the split pvxs draws between two operations: `pvaGetValue`
    /// applies the `sevr` gate only when raising the *owning record's*
    /// `LINK_ALARM` (`pvxs/ioc/pvalink_lset.cpp:424-431` — surfaced here
    /// through [`LinkSet::alarm_severity`]), whereas `pvaGetAlarmMsg`
    /// returns the cached `snap_severity` / `snap_message` snapshot
    /// directly and never consults `sevr`
    /// (`pvxs/ioc/pvalink_lset.cpp:542-575` — surfaced here). A caller
    /// inspecting the DB link's alarm (`dbGetAlarm` / `dbGetAlarmMsg`)
    /// therefore sees the remote severity even on a default `NMS` link
    /// that leaves the owning record unraised.
    ///
    /// `None` means the lset cannot report a snapshot: no cache, the
    /// link is not connected (pvxs `CHECK_VALID` — `pvalink_lset.cpp:545`),
    /// or the link set does not track remote alarms. Default: none.
    fn remote_alarm(&self, _name: &str) -> Option<RemoteAlarm> {
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

/// Per-scheme registry. Held in a snapshot cell inside
/// [`super::PvDatabase`]: readers take an `Arc` of the whole registry with no
/// lock, and `register` rebuilds and republishes under the cell's writer gate
/// (`doc/rtems-priority-locks-design.md` §3 row L8i). `Clone` is what makes
/// that rebuild possible; it is a per-scheme `Arc` clone, not a deep copy.
#[derive(Clone, Default)]
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
    #[async_trait::async_trait]
    impl LinkSet for StubLset {
        fn is_connected(&self, _: &str) -> bool {
            true
        }
        fn get_cached_value(&self, _: &str) -> Option<EpicsValue> {
            Some(EpicsValue::Long(42))
        }
        async fn get_value(&self, name: &str) -> Option<EpicsValue> {
            self.get_cached_value(name)
        }
    }

    #[epics_macros_rs::epics_test]
    async fn register_and_lookup() {
        let mut reg = LinkSetRegistry::new();
        assert!(reg.is_empty());
        reg.register("pva", Arc::new(StubLset));
        assert_eq!(reg.len(), 1);
        let lset = reg.get("pva").expect("registered");
        assert!(lset.is_connected("anything"));
        assert_eq!(lset.get_value("anything").await, Some(EpicsValue::Long(42)));
    }

    #[test]
    fn unknown_scheme_returns_none() {
        let reg = LinkSetRegistry::new();
        assert!(reg.get("missing").is_none());
    }
}
