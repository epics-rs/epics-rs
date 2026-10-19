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
//! Record-link reads dispatch through the matching lset and no other.
//!
//! The trait is **split by thread**, mirroring the C `dbCa` split
//! between the record-processing thread and the `dbCaTask`
//! (`dbCa.c:1093-1260`):
//!
//! * **Synchronous methods** are the ones record processing calls while
//!   it holds the record's advisory write gate (C `dbScanLock`). They
//!   answer from cached, monitor-fed state and MUST NOT perform I/O —
//!   C `dbCaGetLink` copies out of `pca->pgetNative` and never touches
//!   the wire (`dbCa.c:419-506`). Their signature is what enforces
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
//!     fn is_connected(&self, name: &[u8]) -> bool { /* cached */ }
//!     fn get_cached_value(&self, name: &[u8]) -> Option<EpicsValue> { /* cached */ }
//!     async fn get_value(&self, name: &[u8]) -> Option<EpicsValue> { /* may do I/O */ }
//!     /* etc. */
//! }
//! db.register_link_set("pva", Arc::new(MyLset { ... })).await;
//! ```

use std::sync::Arc;

/// An external forward link's `scanForward`, opened once by
/// [`LinkSet::forward_handle`] and held by the link's parse.
///
/// Runs on the record-processing thread with the record held: it MUST NOT
/// block, allocate or resolve a name. pvxs `pvaScanForward`
/// (`pvxs/ioc/pvalink_lset.cpp:672-688`): a forward is never deferred and
/// carries no value; a non-retry link that is not connected triggers
/// nothing and returns [`LinkForwardRefused`], which the caller raises as
/// LINK/INVALID "Disconn" (`:677-680`).
pub trait LinkForward: Send + Sync {
    fn scan_forward(&self) -> Result<(), LinkForwardRefused>;
}

/// The forward was not staged; the source record takes LINK/INVALID.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LinkForwardRefused;

/// [`LinkSet::forward_handle`]'s default.
struct NoForward;

impl LinkForward for NoForward {
    fn scan_forward(&self) -> Result<(), LinkForwardRefused> {
        Ok(())
    }
}

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

/// How an [`LinkPutOp::Async`] put's completion reached the link owner.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkPutDone {
    /// Through the target's completion callback.
    Callback,
    /// On the put's own call: the target processed nothing, so C's
    /// `processNotifyCommon` ran `callDone` inline on `dbCaTask`
    /// (`dbNotify.c:243-270`).
    Inline,
    /// Never: the put callback could not be issued, and C's `dbCaTask` only
    /// logs a failed `ca_array_put_callback`, leaving `newOutNative` set
    /// and never calling `putComplete` (`dbCa.c:1233-1247`).
    NotIssued,
}

/// C `dbCaPutLinkCallback`'s conversion into `pputNative` (`dbCa.c:572-590`):
/// the scalar put row when one element goes to a one-element channel, the
/// array row otherwise, clamped to the channel's `nelements`. A string that
/// is not a number of a numeric `native` type is `Err`, the status
/// `dbPutLink` turns into the link alarm, where C still sends the stale
/// buffer; that write is not copied. `name` labels the error.
pub fn link_put_convert(
    name: &[u8],
    value: EpicsValue,
    native: crate::types::DbFieldType,
    nelements: u32,
) -> Result<EpicsValue, String> {
    use crate::types::c_parse::{self, Converted, NumericField};
    let nelements = nelements as usize;
    // A string put to a DBR_ENUM channel goes as one DBR_STRING, cut by
    // `cvt_st_st`, for the server to resolve (`dbCa.c:561-575`,
    // `dbFastLinkConv.c:74-88`).
    if native == crate::types::DbFieldType::Enum {
        let first = match &value {
            EpicsValue::String(s) => Some(s.as_bytes()),
            EpicsValue::StringArray(a) => a.first().map(|s| s.as_bytes()),
            _ => None,
        };
        if let Some(bytes) = first {
            // `strncpy` into MAX_STRING_SIZE (40) less the NUL.
            let bytes = &bytes[..bytes.len().min(39)];
            let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
            return Ok(EpicsValue::String(crate::types::PvString::from_bytes(
                &bytes[..end],
            )));
        }
    }
    if let Some(row) = NumericField::of(native) {
        let field = String::from_utf8_lossy(name);
        let texts = match &value {
            EpicsValue::String(s) => std::slice::from_ref(s),
            EpicsValue::StringArray(a) => &a[..a.len().min(nelements)],
            _ => &[],
        };
        let converted = match texts {
            [] => None,
            [one] if nelements == 1 => Some(c_parse::put_string(&field, row, &one.as_str_lossy())),
            many => Some(c_parse::put_string_elements(&field, row, many)),
        };
        // `Unchanged` keeps C's old buffer, which this port does not hold.
        if let Some(Converted::Stored(v)) = converted.transpose().map_err(|e| e.to_string())? {
            return Ok(v);
        }
    }
    let mut value = value.into_type(native);
    value.truncate(nelements);
    Ok(value)
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
/// (`dbCa.c:529-532`).
///
/// Answered from cached state only: it runs on a record-processing thread
/// inside the record's advisory write gate, which is exactly where C never
/// touches the network.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PutAdmission {
    /// The lset tracks this link and its channel is up — stage the write.
    /// C's fall-through to `addAction` (`dbCa.c:593`).
    Connected,
    /// The lset tracks this link and will not take the write — the channel
    /// is down, or it is up but the server denies write access. One variant
    /// for both because C has one outcome for both: `-1`, stage nothing
    /// (`dbCa.c:529-532`). The caller raises the owning record's
    /// LINK/INVALID through `dbPutLink`'s `setLinkAlarm`
    /// (`dbLink.c:434-448`).
    ///
    /// Named for the answer rather than for one of its two causes: the
    /// earlier name `Disconnected` was why an implementation of this trait
    /// answered `is_connected()` alone and admitted every write-denied link.
    Refused,
    /// The lset has never opened this link, so it cannot answer. C cannot
    /// reach this state: `dbCaAddLink` opens every CA link at record-init
    /// time (`dbCa.c` `addAction(pca, CA_CONNECT)`), so by the first
    /// `dbCaPutLink` the `caLink` always exists. Our lsets open lazily on
    /// first use instead, so the write is staged and the lset's own
    /// `put_value` performs the open — dropping it would mean an OUT link
    /// that never opens and therefore never connects.
    Unopened,
}

/// One external link's report state — the `caLink` fields `dbcar` prints
/// per link (`dbCaTest.c:95-133`).
///
/// C reads them off one `caLink` struct because dbCa owns the channel, the
/// staged out-value and the connection callback together. Here those belong
/// to two owners — the lset owns the channel and the connection edge, the
/// database's link-put queue owns the staged out-value — so this type
/// carries only the lset's half and `dbcar` joins it with
/// [`PvDatabase::external_link_puts_coalesced_for`], C's `nNoWrite`.
///
/// [`PvDatabase::external_link_puts_coalesced_for`]: super::PvDatabase::external_link_puts_coalesced_for
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LinkDiagnostics {
    /// C's `pca->chid && ca_field_type(pca->chid) != TYPENOTCONN`
    /// (`dbCaTest.c:95-97`) — the channel is up. Not
    /// [`LinkSet::is_connected`], which is the *readable-cache* gate
    /// `dbCaGetLink` applies; a channel that has connected but has yet to
    /// deliver a monitor event counts as connected to `dbcar` and as
    /// unreadable to a record.
    pub connected: bool,
    /// C `ca_host_name(pca->chid)` — the server's name with its port.
    pub host: Vec<u8>,
    /// C `ca_read_access(pca->chid)`.
    pub read_access: bool,
    /// C `ca_write_access(pca->chid)`.
    pub write_access: bool,
    /// C `pca->nDisconnect` (`dbCa.c:822`) — connect→disconnect edges since
    /// the link was opened.
    pub n_disconnect: u64,
    /// `pvlOptInpNative`: the link has served a native input transfer
    /// (`dbCa.c:456`, set on the first `dbCaGetLink` that needs the native
    /// monitor).
    pub input_native: bool,
    /// `pvlOptInpString`: the link has served a *string* input transfer,
    /// which C reaches only for a `DBR_ENUM` channel read as `DBR_STRING`
    /// (`dbCa.c:441`).
    pub input_string: bool,
    /// `pvlOptOutNative` (`dbCa.c:557`). Dead at the pin: the assignment is
    /// inside a `/* Disabled by ANJ ... */` comment, so C never sets it and
    /// `dbcar` always prints the column blank.
    pub output_native: bool,
    /// `pvlOptOutString` (`dbCa.c:541`), disabled by the same ANJ comment.
    pub output_string: bool,
}

/// Remote display / control / valueAlarm metadata snapshot for a
/// link, as exposed by pvxs's pvalink lset metadata getters.
///
/// Mirrors the `pvxs/ioc/pvalink_lset.cpp` metadata getter set installed
/// at `pvxs/ioc/pvalink_lset.cpp:706-732`:
/// `pvaGetDBFtype`, `pvaGetElements`, `pvaGetControlLimits`,
/// `pvaGetGraphicLimits`, `pvaGetAlarmLimits`, `pvaGetPrecision`,
/// `pvaGetUnits`.
///
/// Every field is optional: pvxs's getters read the cached NT
/// structure with `Value::as`, which leaves the caller's buffer
/// unchanged when the sub-field is absent. `None` here means the
/// remote NT value carried no such metadata — the record support
/// then keeps its local/default metadata, exactly as the C path does.
/// The link-backed metadata resolved for **one snapshot build** — the single
/// channel through which a link's target metadata can reach a served
/// [`Snapshot`](crate::server::snapshot::Snapshot).
///
/// The invariant it makes true by construction is *a served snapshot's
/// link-backed metadata was resolved during THIS build*. Three things enforce
/// it, none of them a runtime check:
///
/// * the field is private and [`LinkBacking::resolved`] is `pub(crate)`, so a
///   crate outside `epics-base-rs` cannot make one and must go through
///   [`PvDatabase::channel_snapshot_for_field`](crate::server::database::PvDatabase::channel_snapshot_for_field);
/// * it borrows the resolve's own output, so it cannot outlive the resolve and
///   there is nowhere to *store* it — a stale value is unrepresentable;
/// * `RecordInstance` keeps no link-metadata map, so the consumer has no second
///   source to read.
///
/// C needs no equivalent. `dbLock.c:725-760` merges every DB_LINK-connected
/// record into one lock set behind one recursive mutex, so `get_units` and its
/// siblings call `dbGetUnits`/`dbGetPrecision`/... inline under the target's
/// lock (`dbDbLink.c:240-261`). The port has one `RwLock` per record and no
/// lock sets, so it must resolve with no record lock held and hand the answer
/// in; this type is that hand-off.
#[derive(Clone, Copy)]
pub struct LinkBacking<'a>(Backing<'a>);

/// What one build resolved: nothing, or what its links answered.
#[derive(Clone, Copy)]
enum Backing<'a> {
    Unresolved,
    Resolved(&'a std::collections::HashMap<String, LinkMetadata>),
}

impl<'a> LinkBacking<'a> {
    /// Nothing was resolved for this build: the record backs no field's
    /// metadata with a link, or the caller is a path that serves only
    /// non-link-backed fields. Every lookup answers `None`, which serves each
    /// rset slot's C seed — the same answer C's untouched buffer gives for a
    /// CONSTANT link, an unresolvable target, or a link its
    /// `DBLINK_FLAG_VISITED` guard refused.
    pub const fn none() -> Self {
        Self(Backing::Unresolved)
    }

    /// What the links this record's metadata is backed by resolved to, keyed
    /// by link field (`INPA`, `INPB`, ...), resolved with no record lock held.
    pub const fn resolved(resolved: &'a std::collections::HashMap<String, LinkMetadata>) -> Self {
        Self(Backing::Resolved(resolved))
    }

    /// The metadata behind one link field. The *predicate* — whether this
    /// served field is link-backed at all — stays with
    /// `Record::link_backed_metadata_field`, so this type answers only the
    /// value and carries one meaning.
    pub(crate) fn metadata(&self, link_field: &str) -> Option<&'a LinkMetadata> {
        match self.0 {
            Backing::Resolved(m) => m.get(link_field),
            Backing::Unresolved => None,
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct LinkMetadata {
    /// DBF type the remote value maps to (`pvaGetDBFtype`). A connected
    /// link always reports a type — an unmappable value shape falls back
    /// to `Long`, the `default:` arm of pvxs `pvaGetDBFtype`
    /// (`pvxs/ioc/pvalink_lset.cpp:199-236`). `None` therefore means "not
    /// connected" (no cached value), never "connected but unmappable".
    pub dbf_type: Option<LinkDbfType>,
    /// Element count: array length, or `1` for a scalar / any connected
    /// non-array shape (`pvaGetElements`, `pvxs/ioc/pvalink_lset.cpp:242-257`).
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
/// link's `sevr` mode (`pvxs/ioc/pvalink_lset.cpp:542-569`; `pvaGetAlarm`
/// `:571-575` is the thin wrapper that calls it with no message buffer).
/// A default
/// `NMS` link must still report its remote severity here even though it
/// does not maximize the owning record's severity.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct RemoteAlarm {
    /// Remote alarm severity (`0 = NO_ALARM` … `3 = INVALID`), the raw
    /// cached `alarm.severity` — never gated by the link's `sevr` mode.
    pub severity: i32,
    /// Remote alarm status code, derived from `severity` exactly as
    /// pvxs `pvaGetAlarmMsg` does (`LINK_ALARM` when severity is
    /// non-`NO_ALARM`, else `NO_ALARM` — `pvxs/ioc/pvalink_lset.cpp:554`). See
    /// [`RemoteAlarm::from_severity_message`].
    pub status: i32,
    /// Remote `alarm.message`. Empty when the remote carried none or
    /// the severity is `NO_ALARM` (pvxs clears `snap_message` unless
    /// `snap_severity != 0` — `pvxs/ioc/pvalink_lset.cpp:418-422`). Shared
    /// with the lset's cache, as [`LinkSet::alarm_message`].
    pub message: Arc<str>,
}

/// The record field a link lives in — C's `plink->precord` and
/// `dbLinkFieldName(plink)`, which pvxs `pvaOpenLink` keeps on its `pvaLink`
/// to scan the holder on a monitor event (`pvalink_lset.cpp:91-92`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LinkHolder<'a> {
    pub record: &'a [u8],
    pub field: &'a str,
    class: Option<crate::types::DbfLinkClass>,
}

impl<'a> LinkHolder<'a> {
    /// `record`'s `field`, of a `record_type` record.
    pub fn new(record_type: &str, record: &'a [u8], field: &'a str) -> Self {
        Self {
            record,
            field,
            class: crate::types::dbf_link_class(record_type, field),
        }
    }

    /// The field's declared link class — what pvxs keeps as
    /// `pvaLink::type` (`getLinkType`, `pvalink_lset.cpp:52-62`). `None` for
    /// a field the type does not declare as a link.
    pub fn class(&self) -> Option<crate::types::DbfLinkClass> {
        self.class
    }
}

/// What an lset's [`LinkSet::open_link`] left the link with.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LinkOpen {
    /// The lset serves the link.
    Lset,
    /// The lset set `plink->lset = NULL` (`pvalink_lset.cpp:70-75`).
    NoLset,
}

impl RemoteAlarm {
    /// Build a snapshot whose `status` is derived from `severity`
    /// exactly as pvxs `pvaGetAlarmMsg` (`pvxs/ioc/pvalink_lset.cpp:554`):
    /// `LINK_ALARM` when the remote severity is non-`NO_ALARM`, else
    /// `NO_ALARM`. status and severity cannot disagree by construction.
    pub fn from_severity_message(severity: i32, message: Arc<str>) -> Self {
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
///
/// A link `name` is the bytes of the link's PV name, as C hands
/// `pv_link.pvname` to `dbCaAddLink`: a CA link may name a record that is
/// not UTF-8.
#[async_trait::async_trait]
pub trait LinkSet: Send + Sync {
    /// True iff a fresh value is available for `name` without
    /// blocking. Used by the record processing loop to decide
    /// whether to mark the record's STAT as LINK_ALARM.
    ///
    /// Synchronous: asked on the record-processing thread inside the
    /// record's advisory write gate. MUST NOT perform I/O.
    fn is_connected(&self, name: &[u8]) -> bool;

    /// True iff `name` has completed every post-connect init action
    /// `dbCaRun`'s wait holds for — C `testInitReady`
    /// (`dbCa.c:835-845` at `ef4829829`, epics-base #856 "dbCa: iocInit
    /// wait for all conditions" — post-`R7.0.10` and in no tag, so this
    /// is the one citation here that is not pin-relative): connected with
    /// the first monitor event cached AND
    /// the attribute (metadata) fetch complete. Distinct from
    /// [`Self::is_connected`], which is C's lset `isConnected` and keeps
    /// its readable-cache semantics.
    ///
    /// Synchronous and non-blocking like `is_connected`; polled only by
    /// the iocInit wait. Default: `is_connected` — right for an lset
    /// with no post-connect init actions.
    fn init_ready(&self, name: &[u8]) -> bool {
        self.is_connected(name)
    }

    /// Read the current value of `name`. Returns None when the
    /// upstream isn't yet connected or the lset has no cache for
    /// this name.
    ///
    /// MAY perform I/O (open the channel, issue a one-shot GET). It is
    /// therefore called only from the database's link work owner task —
    /// the record-processing path uses [`Self::get_cached_value`].
    async fn get_value(&self, name: &[u8]) -> Option<EpicsValue>;

    /// Read `name` from cached, monitor-fed state ONLY — the
    /// record-processing read. C `dbCaGetLink` (`dbCa.c:419-506`) copies
    /// out of `pca->pgetNative`, the buffer the CA monitor callback
    /// (`eventCallback`, `dbCa.c:891-967`, the fill at `:941-944`)
    /// keeps fresh on the `dbCaTask`; it never opens a channel and never
    /// waits on the wire. Returns None
    /// when the link has no cached value yet, which is C returning -1 for
    /// `!pca->isConnected` (`dbCa.c:430-435`) — the reading record takes
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
    fn get_cached_value(&self, name: &[u8]) -> Option<EpicsValue> {
        let _ = name;
        None
    }

    /// Whether this lset's `getValue` raised `"<field> Disconn"` on the
    /// reading record when its read of `name` failed, as pvalink's
    /// `pvaGetValue` does on a link that is not valid
    /// (`pvalink_lset.cpp:263-268`) — before `dbGetLink`'s `setLinkAlarm`,
    /// whose equal severity then leaves the message alone (`recGbl.c:242`).
    /// Asked only after a failed read; MUST NOT perform I/O.
    ///
    /// Default `false`: `dbCaGetLink` raises nothing itself
    /// (`dbCa.c:459-463`).
    fn get_failure_disconn(&self, name: &[u8]) -> bool {
        let _ = name;
        false
    }

    /// C `lset->openLink` (`dbLink.c:194-199`, from `dbJLinkInit`,
    /// `dbJLink.c:459`): where the lset settles what `name` is, asked of a
    /// link naming this lset's scheme (`pva://`, `{pva:…}`), once per
    /// link at iocInit (`doResolveLinks`, ahead of `init_record` pass 1,
    /// `iocInit.c:576-577`) and once per put of the link, under the put's
    /// hold where `dbPutFieldLink` runs `dbAddLink` (`dbAccess.c:1208`).
    /// Synchronous and MUST NOT perform I/O; it may allocate and post to
    /// errlog, as pvalink's open does (`pvalink_lset.cpp:64-75`).
    /// [`LinkOpen::NoLset`] leaves the link [`ParsedLink::Removed`], which
    /// every path treats as C treats `plink->lset == NULL` (`dbLink.c:224`,
    /// `:336`, `:440`, `:479`). A local-PV test answers through
    /// [`PvDatabase::has_name_no_resolve`](super::PvDatabase::has_name_no_resolve).
    ///
    /// [`ParsedLink::Removed`]: crate::server::record::ParsedLink::Removed
    fn open_link(&self, holder: LinkHolder<'_>, name: &[u8]) -> LinkOpen {
        let _ = (holder, name);
        LinkOpen::Lset
    }

    /// C `lset->removeLink` (`dbLink.c:202-213`), from `dbPutFieldLink`'s
    /// `dbRemoveLink` (`dbAccess.c:1167`): `holder` no longer carries the
    /// link `name` that [`Self::open_link`] opened for it. Asked under the
    /// put's hold, once the put has replaced or removed the link, before the
    /// open of whatever replaced it. MUST NOT perform I/O.
    fn remove_link(&self, holder: LinkHolder<'_>, name: &[u8]) {
        let _ = (holder, name);
    }

    /// Open (subscribe / connect) `name` so later
    /// [`Self::get_cached_value`] reads have a cache to serve — C
    /// `dbCaAddLink` (`dbCa.c:397-401`), which stages a `CA_CONNECT`
    /// action whose `ca_create_channel` +
    /// `ca_add_array_event` run on the `dbCaTask`, not on the caller.
    ///
    /// **Called from the database's link work owner task**, so it MAY
    /// block on the network. Idempotent: the owner may call it again for
    /// a link that is already open or still connecting.
    ///
    /// More precisely, it is called on the tokio runtime the database
    /// captured at construction, so `tokio::net` is usable here — and that
    /// is the *only* place it is usable. A database built with no runtime
    /// entered anywhere captured none, and there is no second executor that
    /// could stand in: the process-global background executor deliberately
    /// carries no `tokio::net` reactor. Such a database therefore never
    /// calls this method at all; it refuses the link instead
    /// (`PvDatabase::external_put_gate`). Do not read the absence of a
    /// runtime as a reason to open the channel synchronously on the
    /// caller — there is no caller thread that may block that way.
    ///
    /// `init` is set for a local CA link opened before `dbCaRun`, which
    /// waits until every clone of it drops: an lset that serves the link
    /// from a local channel holds it until the first event's process has
    /// run (C `CA_INIT_READY`, `dbCa.c:1323-1330`).
    ///
    /// Default: drive the lset's own lazy open by reading through
    /// [`Self::get_value`] and discarding the result — correct for every
    /// existing lset, and it runs off the record-processing thread.
    async fn connect_link(&self, name: &[u8], init: Option<super::CaLinkInit>) {
        drop(init);
        let _ = self.get_value(name).await;
    }

    /// The CA client a `ca` network half runs its links on, for the
    /// channels a record opens itself ([`super::CaClientContext`]).
    ///
    /// Default: none.
    fn ca_client_service(&self) -> Option<Arc<dyn super::CaClientService>> {
        None
    }

    /// A CP/CPP link of `holder` was added after iocInit to `name`. C gives
    /// the new link its own `caLink`, whose first event processes `holder`
    /// (`dbCa.c:958-962`); an lset sharing one channel per name must
    /// request that process itself when the channel has already delivered
    /// its first event, and leave it to that event otherwise.
    ///
    /// Default: nothing — an lset with a channel per link gets the event.
    fn cp_link_added(&self, name: &[u8], holder: super::CpTarget) {
        let _ = (name, holder);
    }

    /// Non-blocking admission gate for an OUT-link write, asked on the
    /// record-processing thread *before* the write is staged onto the
    /// database's link-put queue — C `dbCaPutLinkCallback`'s
    /// `if (!pca->isConnected || !pca->hasWriteAccess) return -1;`
    /// (`db/dbCa.c:529-532` (`dbCaPutLinkCallback`); epics-base R7.0.10).
    ///
    /// MUST NOT perform I/O. It is the one lset call left inside the
    /// record's advisory write gate, and the whole point of the queue is
    /// that nothing there touches the network.
    ///
    /// Default: derive from [`Self::is_connected`], which the trait already
    /// documents as answerable "without blocking" — i.e. only C's FIRST
    /// operand. An lset whose protocol also carries a write right MUST
    /// override and test both, because the default cannot see the second
    /// one; so MUST an lset whose OUT links live in a different cache than
    /// its INP links (pvalink keys its registry on direction), or every OUT
    /// write to a perfectly healthy channel is refused.
    fn put_admission(&self, name: &[u8]) -> PutAdmission {
        if self.is_connected(name) {
            PutAdmission::Connected
        } else {
            PutAdmission::Refused
        }
    }

    /// The native type and element count a put to `name` is converted to
    /// on the record thread, C `pca->dbrType` / `pca->nelements`
    /// (`dbCa.c:572-590`), from cached state like [`Self::put_admission`].
    /// `None`: the link is down, or the lset converts in
    /// [`Self::put_value`] itself (the default).
    fn put_native(&self, name: &[u8]) -> Option<(crate::types::DbFieldType, u32)> {
        let _ = name;
        None
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
    /// split (`db/dbCa.c:1161-1183` (`dbCaTask`); epics-base R7.0.10), so it
    /// may block on the network.
    ///
    /// As with [`Self::connect_link`], "may block on the network" means "on
    /// the tokio runtime the database captured", which is where the owner
    /// dispatches it. A database that captured no runtime never reaches this
    /// method: the write is refused at `PvDatabase::external_put_gate` with
    /// nothing staged, C's shape for a put it cannot deliver
    /// (`db/dbCa.c:529-532` (`dbCaPutLinkCallback`); R7.0.10).
    async fn put_value(&self, name: &[u8], value: EpicsValue, op: LinkPutOp) -> Result<(), String> {
        let _ = (name, value, op);
        Err("link set is read-only".into())
    }

    /// [`Self::put_value`], saying how its completion arrived, refused or
    /// not. Default: [`LinkPutDone::Callback`], which only a local CA target
    /// can refute.
    async fn put_value_done(
        &self,
        name: &[u8],
        value: EpicsValue,
        op: LinkPutOp,
    ) -> (LinkPutDone, Result<(), String>) {
        (LinkPutDone::Callback, self.put_value(name, value, op).await)
    }

    /// Open `name` as a forward link: the handle its `scanForward` runs
    /// through, C `lset->openLink` at `dbInitLink`/`dbAddLink` time, so a
    /// dispatch resolves nothing (`dbLink.c:475`, `scanForward(plink)`).
    ///
    /// Called off the cycle path only, by the database's binding owner
    /// (`PvDatabase::bind_external_forward`). Default: a handle that
    /// forwards nothing — a read-only or DB-local lset has no
    /// `scanForward`.
    fn forward_handle(&self, name: &[u8]) -> Box<dyn LinkForward> {
        let _ = name;
        Box::new(NoForward)
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
    /// (`pvxs/ioc/pvalink_lset.cpp:653`, `pvxs/ioc/pvalink_channel.cpp:220-280`).
    async fn flush_puts(&self) {}

    /// Most recent alarm message string from the upstream PV, when
    /// available. None means no alarm or no cache.
    ///
    /// The lset hands out the text it cached when the remote value arrived:
    /// an alarmed link's every cycle stores it into the record's `namsg`
    /// (`recGblSetSevrMsg`, `pvalink_lset.cpp:432-434`), and a shared text
    /// makes that store a pointer compare. A new text is allocated once per
    /// change of the remote message, never per call.
    fn alarm_message(&self, _name: &[u8]) -> Option<Arc<str>> {
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
    /// `pvxs/ioc/pvalink_lset.cpp` `pvaGetAlarm` feeding `recGblSetSevr`.
    fn alarm_severity(&self, _name: &[u8]) -> Option<i32> {
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
    /// this default. Mirrors `pvxs/ioc/pvalink_lset.cpp` `pvaGetAlarm`
    /// surfacing the remote `alarm.status` to `recGblSetSevrMsg`.
    fn alarm_status(&self, _name: &[u8]) -> Option<i32> {
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
    /// (`pvxs/ioc/pvalink_lset.cpp:542-569`, with `pvaGetAlarm` `:571-575`
    /// its no-message-buffer wrapper — surfaced here). A caller
    /// inspecting the DB link's alarm (`dbGetAlarm` / `dbGetAlarmMsg`)
    /// therefore sees the remote severity even on a default `NMS` link
    /// that leaves the owning record unraised.
    ///
    /// `None` means the lset cannot report a snapshot: no cache, the
    /// link is not connected (pvxs `CHECK_VALID` — `pvxs/ioc/pvalink_lset.cpp:548`),
    /// or the link set does not track remote alarms. Default: none.
    fn remote_alarm(&self, _name: &[u8]) -> Option<RemoteAlarm> {
        None
    }

    /// `(seconds_past_epoch, nanoseconds, userTag)` from the upstream
    /// PV's timestamp slot, when available. The `userTag` is the remote
    /// `timeStamp.userTag` widened to the 64-bit `epicsUTag` tag without
    /// sign extension, or `0` when the source carries none (CA links, or
    /// a PVA source whose timeStamp omits the field).
    fn time_stamp(&self, _name: &[u8]) -> Option<(i64, i32, u64)> {
        None
    }

    /// Remote display / control / valueAlarm metadata for `name`, as
    /// a single snapshot.
    ///
    /// The Rust counterpart of pvxs's pvalink lset metadata getter
    /// set (`pvaGetDBFtype`, `pvaGetElements`, `pvaGetControlLimits`,
    /// `pvaGetGraphicLimits`, `pvaGetAlarmLimits`, `pvaGetPrecision`,
    /// `pvaGetUnits` — installed at `pvxs/ioc/pvalink_lset.cpp:706-732`).
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
    fn link_metadata(&self, _name: &[u8]) -> Option<LinkMetadata> {
        None
    }

    /// Enumerate every PV name this lset has *opened* (i.e., is
    /// actively tracking). Used by `dbpvxr` to dump per-record
    /// link state without forcing the caller to know the full
    /// name list up-front.
    fn link_names(&self) -> Vec<crate::types::RecordName> {
        Vec::new()
    }

    /// This link's `dbcar` report state, or `None` when the lset has never
    /// opened `name` — C's `pca == NULL`, which `dbcar` prints as a
    /// not-connected link with zero counters (`dbCaTest.c:127-132`).
    ///
    /// Async because C's host field is `ca_host_name(pca->chid)`, and this
    /// port's twin (`CaChannel::host_name`) resolves the peer's PTR record
    /// on a blocking thread exactly as libca's `hostNameCache` does. That
    /// makes this the one `LinkSet` method neither half of the C split owns:
    /// it is a diagnostic, called from iocsh, never from record processing
    /// and never from the link work owner.
    ///
    /// Default: `None`, i.e. "this lset has no per-link report state".
    /// Such an lset's links are invisible to `dbcar`, which is right — C's
    /// `dbcar` walks `plink->type == CA_LINK` and no other link flavour.
    async fn link_diagnostics(&self, _name: &[u8]) -> Option<LinkDiagnostics> {
        None
    }
}

/// Type-erased lset reference held by the [`LinkSetRegistry`].
pub type DynLinkSet = Arc<dyn LinkSet>;

/// Per-scheme registry. Held in a snapshot cell inside
/// [`super::PvDatabase`]: readers take an `Arc` of the whole registry with no
/// lock, and `register` rebuilds and republishes under the cell's writer gate.
/// `Clone` is what makes that rebuild possible; it is a per-scheme `Arc`
/// clone, not a deep copy.
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
        self.get_ref(scheme).cloned()
    }

    pub(crate) fn get_ref(&self, scheme: &str) -> Option<&DynLinkSet> {
        self.inner.get(scheme)
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

    /// Boundaries of `dbCaPutLinkCallback`'s two conversion rows: the scalar
    /// row (one element to a one-element channel) takes the empty string as
    /// 0, the array row refuses it, and a refused element refuses the put.
    #[test]
    fn link_put_convert_picks_cs_row_by_count() {
        use crate::types::{DbFieldType as T, PvString};
        let conv = |v, t, n| link_put_convert(b"RMT", v, t, n);
        let s = |t: &str| EpicsValue::String(t.into());
        let strings =
            |ts: &[&str]| EpicsValue::StringArray(ts.iter().map(|&t| PvString::from(t)).collect());
        assert!(conv(s("abc"), T::Double, 1).is_err());
        assert_eq!(conv(s("0x10"), T::Long, 1), Ok(EpicsValue::Long(16)));
        assert_eq!(conv(s(""), T::Double, 1), Ok(EpicsValue::Double(0.0)));
        assert!(conv(s(""), T::Double, 3).is_err());
        assert!(conv(strings(&["1", "x"]), T::Long, 2).is_err());
        // Clamped before converting: the element past `nelements` is not read.
        assert_eq!(
            conv(strings(&["1", "2", "x"]), T::Long, 2),
            Ok(EpicsValue::LongArray(vec![1, 2].into()))
        );
        assert_eq!(conv(s("abc"), T::String, 1), Ok(s("abc")));
        assert_eq!(
            conv(EpicsValue::Double(2.5), T::Long, 1),
            Ok(EpicsValue::Long(2))
        );
    }

    /// Boundary: a string to an enum channel stays one string, the first
    /// element of an array, cut to 39 bytes as `cvt_st_st` cuts it.
    #[test]
    fn link_put_convert_sends_a_string_to_an_enum_channel_as_one_string() {
        use crate::types::{DbFieldType as T, PvString};
        let conv = |v| link_put_convert(b"RMT", v, T::Enum, 1);
        let s = |t: &str| EpicsValue::String(t.into());
        assert_eq!(conv(s("two")), Ok(s("two")));
        assert_eq!(
            conv(EpicsValue::StringArray(
                ["one", "two"].iter().map(|&t| PvString::from(t)).collect()
            )),
            Ok(s("one"))
        );
        assert_eq!(conv(s(&"x".repeat(45))), Ok(s(&"x".repeat(39))));
        assert_eq!(conv(EpicsValue::Long(2)), Ok(EpicsValue::Enum(2)));
    }

    struct StubLset;
    #[async_trait::async_trait]
    impl LinkSet for StubLset {
        fn is_connected(&self, _: &[u8]) -> bool {
            true
        }
        fn get_cached_value(&self, _: &[u8]) -> Option<EpicsValue> {
            Some(EpicsValue::Long(42))
        }
        async fn get_value(&self, name: &[u8]) -> Option<EpicsValue> {
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
        assert!(lset.is_connected(b"anything"));
        assert_eq!(
            lset.get_value(b"anything").await,
            Some(EpicsValue::Long(42))
        );
    }

    #[test]
    fn unknown_scheme_returns_none() {
        let reg = LinkSetRegistry::new();
        assert!(reg.get("missing").is_none());
    }

    /// A subscriber that attaches after a put reads the record and before it
    /// posts receives the post, link-backed field or not: C's
    /// `db_post_events` posts to whoever is attached when it runs, and a post
    /// carries no metadata a later reader could find missing.
    #[epics_macros_rs::epics_test]
    async fn a_late_subscriber_receives_the_post() {
        use crate::server::recgbl::EventMask;
        use crate::types::DbFieldType;

        let db = crate::server::database::PvDatabase::new();
        let src = crate::server::records::ai::AiRecord::new(1.0);
        db.add_record("SRC", Box::new(src)).await.unwrap();
        let mut calc = crate::server::records::calc::CalcRecord::default();
        calc.set_inp_link(0, "SRC");
        calc.calc = "A+1".into();
        db.add_record("CALC", Box::new(calc)).await.unwrap();
        db.ioc_init().await;

        let rec = db.get_record("CALC").expect("record exists");
        let full = (EventMask::VALUE | EventMask::LOG).bits();
        let mut a_rx = rec
            .write()
            .add_subscriber("A", 1, DbFieldType::Double, full)
            .expect("A subscriber");
        let mut val_rx = rec
            .write()
            .add_subscriber("VAL", 2, DbFieldType::Double, full)
            .expect("VAL subscriber");

        rec.write()
            .notify_field_with_origin("A", EventMask::VALUE, 0);
        rec.write()
            .notify_field_with_origin("VAL", EventMask::VALUE, 0);

        assert!(a_rx.try_recv().is_ok(), "A is link-backed and still posts");
        assert!(val_rx.try_recv().is_ok(), "VAL posts");
    }
}
