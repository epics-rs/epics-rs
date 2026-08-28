use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use crate::error::{CaError, CaResult};
use crate::server::database::LinkBacking;
use crate::server::event_queue::{EventReader, EventUser};
use crate::server::pv::{MonitorEvent, Subscriber};
use crate::server::recgbl::EventMask;
use crate::server::snapshot::{
    ControlInfo, DisplayInfo, EnumInfo, EnumStringForm, PropertySupport,
};
use crate::types::c_parse::Converted;
use crate::types::{DbFieldType, EpicsValue, PvString, c_parse};

use super::alarm::{AlarmLimit, AlarmSeverity, AnalogAlarmConfig};
use super::common_fields::CommonFields;
use super::link::{
    ParsedLink, out_link_discards_cp, parse_forward_link_v2, parse_link_v2, parse_output_link_v2,
};
use super::menu_choices::MenuBound;
use super::record_trait::{
    AuxPostMask, CommonFieldPutResult, FieldDeclaration, FieldDesc, ProcessSnapshot, Record,
    RecordProcessResult, SubroutineFn,
};
use super::scan::{ScanType, SimModeScan};

/// C `msstring[4]` (`dbStaticLib.c:61`) — the maximize-severity word
/// `dbGetString` appends to a link and `dblsr` prints in its own column.
pub(crate) fn monitor_switch_word(switch_: super::MonitorSwitch) -> &'static str {
    use super::MonitorSwitch::*;
    match switch_ {
        NoMaximize => "NMS",
        Maximize => "MS",
        MaximizeIfInvalid => "MSI",
        MaximizeStatus => "MSS",
    }
}

/// C `dbGetString`'s three link branches (`dbStaticLib.c:1906-2050`) — how a
/// link field READS, as opposed to how it is stored.
///
/// C keeps a link field as a parsed `struct link` in record memory and renders
/// it on every read, so the modifiers a `.db` left out come back as their
/// defaults: `L:B` in an `INP` reads `L:B NPP NMS`, and `L:B PP MS` in a
/// `FLNK` reads `L:B`, because `DBF_FWDLINK` carries no process class and no
/// severity switch. Measured against `softIoc` R7.0.10-146 over CA.
///
/// This port stores the text instead, so the rendering happens on the way out.
/// It is applied once, in [`RecordInstance::resolve_field`], and never in a
/// printer: the CA server, `dbgf`, `dbpf`'s read-back and `dbpr` all read
/// through that funnel, and a rule kept in three printers is a rule the fourth
/// reader does not get.
///
/// The target is the slice before the FIRST space rather than a name rebuilt
/// from the parse, because that is what C stores: `dbParseLink` splits there
/// and keeps the head verbatim in `pv_link.pvname`. `X.VAL` therefore prints
/// as `X.VAL`, not as the `X` a round trip through `DbLink::channel_name`
/// would produce.
///
/// Only PV/DB/CA links carry modifiers, so every other link type falls through
/// to its own text — CONSTANT prints `constantStr` (`:1911-1917`), JSON_LINK
/// the JSON (`:1927`). Hardware links are the exception that is not a
/// fall-through: C stores them as numbers and re-renders them per bus
/// (`:1953-2006`), so this funnel asks [`HwLink::render`](super::HwLink::render)
/// rather than echoing the field text, and `#C0x10 S-2` reads back as
/// `#C16 S-2 @`.
///
/// Rendering is idempotent under the parser — `L:B NPP NMS` parses to the link
/// `L:B` does — so a consumer that re-parses what a reader saw gets the link
/// the store holds.
pub(crate) fn render_link_field(class: crate::types::DbfLinkClass, raw: &str) -> String {
    use super::{LinkFieldType, LinkProcessPolicy};
    use crate::types::DbfLinkClass;

    let text = raw.trim();
    let ftype = LinkFieldType::for_class(class);
    // The parse already applied C's per-field-type modifier mask
    // (`dbStaticLib.c:2380-2391`), so a `DBF_FWDLINK` reaching the arm below
    // has had everything but `CA` cleared and cannot render a stale ` MS`.
    let (policy, ms, ca_class) = match super::parse_link_field(text, ftype) {
        ParsedLink::Db(link) => (link.policy, link.monitor_switch, false),
        ParsedLink::Ca(link) => (link.policy, link.monitor_switch, true),
        // The store holds the parsed bus numbers, so the text comes from
        // them and from nowhere else.
        ParsedLink::Hw(hw) => return hw.render(),
        _ => return text.to_string(),
    };
    let target = text.split_once(' ').map_or(text, |(head, _)| head);

    // A forward link prints its target and, alone among the modifiers, ` CA`
    // (`dbStaticLib.c:2034-2044`): no process class and no maximize-severity
    // switch, which is why C answers a bare `FLNK` with just the record name.
    if matches!(class, DbfLinkClass::FwdLink) {
        return if ca_class {
            format!("{target} CA")
        } else {
            target.to_string()
        };
    }

    // C's `ppind` chain (`:1938-1943`) tests `PP` before `CA`, so a `ca://`
    // link that also asked for `PP` renders ` PP`; and a `CP`/`CPP` link that
    // resolved to a CA channel still renders its own class, because C reads
    // `pvlMask` and not the type the link ended up with.
    let pp = if ca_class && policy == LinkProcessPolicy::NoProcess {
        " CA"
    } else {
        match policy {
            LinkProcessPolicy::NoProcess => " NPP",
            LinkProcessPolicy::ProcessPassive => " PP",
            LinkProcessPolicy::ChannelProcess => " CP",
            LinkProcessPolicy::ChannelProcessPassive => " CPP",
        }
    };
    format!("{target}{pp} {}", monitor_switch_word(ms))
}

/// Every client-visible `special(SPC_NOMOD)` field of `dbCommon.dbd:13-190`.
///
/// These are common fields — no record's `field_list` declares them — so the
/// declaration names them here. The remaining `SPC_NOMOD` entries in
/// `dbCommon.dbd` are `DBF_NOACCESS` ([`is_dbcommon_noaccess`]): they have
/// no field API in this port at all.
///
/// TIME is `DBF_NOACCESS` in C, and so it is here — [`FieldDesc::unreadable`]
/// refuses the read. It is still named here because `SPC_NOMOD` is a fact about
/// the declaration, not about readability: C's `dbCommon.dbd` marks it
/// `special(SPC_NOMOD)` and every write path must see that whether or not any
/// read path ever succeeds.
///
/// [`FieldDesc::unreadable`]: super::FieldDesc::unreadable
///
/// Read only through [`RecordInstance::is_no_mod`].
const DBCOMMON_NOMOD: &[&str] = &[
    "NAME", "STAT", "SEVR", "AMSG", "NSTA", "NSEV", "NAMSG", "ACKS", "ACKT", "LCNT", "PACT",
    "PUTF", "RPRO", "TIME", "UTAG",
];

/// Is `field` (already uppercased) a `dbCommon` `DBF_NOACCESS` internal —
/// a name C resolves but never serves?
///
/// These are C-internal pointers with no value API in this port. Their NAMES
/// still exist to C's resolver: `dbNameToAddr` resolves a `DBF_NOACCESS`
/// field and the refusal lands at channel *creation*, where `mapDBFToDBR`
/// yields `DBR_NOACCESS` — measured against `softIocPVX`:
/// `pvxget ORACLE:AI.MLOK` → `Refused to create Channel`, i.e. the SEARCH
/// was answered. The search gate (`PvDatabase::has_name_no_resolve`)
/// consults this — via [`RecordInstance::resolves_noaccess_name`] — so those
/// names keep answering; every *value* path stays closed to them.
///
/// The name list is the generated spec
/// ([`DB_COMMON_NOACCESS`](super::dbd_generated::DB_COMMON_NOACCESS)) minus
/// [`DBCOMMON_NOMOD`], and it holds only the rows the generator could state no
/// width for. `BKPT` and `TIME` are NOT in it: their `extra(...)` names a plain
/// scalar, so the generator carries the whole descriptor and the search gate is
/// answered by its `field_desc` arm instead. Both arms answer the SEARCH; which
/// one does is a property of the declaration, not of this function.
pub(crate) fn is_dbcommon_noaccess(field: &str) -> bool {
    super::dbd_generated::DB_COMMON_NOACCESS.contains(&field) && !DBCOMMON_NOMOD.contains(&field)
}

thread_local! {
    /// The origin tag applied to every event posted from the current
    /// thread's synchronous put+process cascade when the poster itself
    /// passes origin 0. Set only by [`AmbientWriteOriginScope`], read only
    /// by [`RecordInstance::notify_field_with_origin`]. An in-process
    /// writer (a ported SNL state machine) uses this so the whole
    /// synchronous consequence of its put — the direct field post AND the
    /// process-cycle posts, FLNK cascade included — carries its origin and
    /// is filtered from its own subscriptions, while posts from work the
    /// cascade merely *spawned* (a motor poller on another task) stay
    /// untagged and visible to it.
    static AMBIENT_WRITE_ORIGIN: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// RAII scope for `AMBIENT_WRITE_ORIGIN`. Sound only around code with no
/// `.await` inside: the tag is thread-local, so crossing an await point
/// would both leak it to interleaved tasks and lose it on work-stealing.
/// The put paths that use it (`put_record_field_from_ca_no_notify_with_origin`)
/// wrap a fully synchronous body.
pub struct AmbientWriteOriginScope {
    prev: u64,
}

/// Enter an ambient-origin scope; the previous value is restored on drop
/// (scopes nest).
pub fn ambient_write_origin_scope(origin: u64) -> AmbientWriteOriginScope {
    let prev = AMBIENT_WRITE_ORIGIN.with(|c| c.replace(origin));
    AmbientWriteOriginScope { prev }
}

impl Drop for AmbientWriteOriginScope {
    fn drop(&mut self) {
        AMBIENT_WRITE_ORIGIN.with(|c| c.set(self.prev));
    }
}

/// The current thread's ambient write origin (0 outside any scope).
/// `pub(crate)` so the simple-PV posting funnel
/// (`ProcessVariable::deliver`) applies the same inheritance rule as the
/// two record funnels in this file.
pub(crate) fn ambient_write_origin() -> u64 {
    AMBIENT_WRITE_ORIGIN.with(|c| c.get())
}

/// Put-notify completion wait-set — the C `dbNotify.c` `processNotify`
/// waitList analogue (`dbNotifyAdd` / `dbNotifyCompletion`).
///
/// A `ca_put_callback` / WRITE_NOTIFY completion must fire only after the
/// originating (put-target) record AND every record reached through its
/// FLNK / OUT / process-action dispatch chain (synchronous *or* async)
/// has finished processing. A single wait-set owns the completion
/// oneshot; only it fires, and only when the last chain member leaves.
///
/// Counting convention: [`Self::new`] arms `pending = 1` for the
/// originating record (which always joins). Every additional PP target
/// that will process under the active notify [`Self::enter`]s on join
/// (C `dbNotifyAdd`), and every record [`Self::leave`]s when its
/// processing completes (C `dbNotifyCompletion`). The oneshot fires on
/// the `leave` that drops `pending` to zero.
pub struct NotifyWaitSet {
    pending: AtomicUsize,
    tx: StdMutex<Option<crate::runtime::sync::oneshot::Sender<()>>>,
    /// C `dbChannelRecord(ppn->chan)` — the record the notify was ISSUED
    /// against, as opposed to the records that joined its chain.
    ///
    /// `None` for a set that is not a `processNotify` at all: the
    /// completion accounting [`PvDatabase::new_put_notify`] arms for a
    /// downstream link put has no `chan`, so C has no such record and
    /// `dbNotifyDump` has no block to print for it.
    ///
    /// Set at construction and never afterwards. The only mint of an
    /// entry-bearing set is [`RecordInstance::install_or_queue_notify`],
    /// which passes its own record, and the only other writer of the slot
    /// ([`RecordInstance::join_put_notify`]) clones a set it did not make —
    /// so "the entry names the record whose slot minted it" holds by
    /// construction rather than by a check.
    ///
    /// [`PvDatabase::new_put_notify`]: crate::server::database::PvDatabase::new_put_notify
    entry: Option<Box<str>>,
}

impl NotifyWaitSet {
    /// Arm a wait-set whose `tx` fires when the chain settles. `pending`
    /// starts at 1 for the originating record — its completion `leave`s
    /// that implicit slot, so a put with no chain targets fires
    /// immediately on the originating record's own completion.
    ///
    /// No entry record: this is the chain-internal set, C's `dbNotifyAdd`
    /// bookkeeping without a `processNotify` of its own.
    /// `Self::for_entry_record` is the `dbProcessNotify` arm.
    pub fn new(tx: crate::runtime::sync::oneshot::Sender<()>) -> Arc<Self> {
        Arc::new(Self {
            pending: AtomicUsize::new(1),
            tx: StdMutex::new(Some(tx)),
            entry: None,
        })
    }

    /// C `dbProcessNotify` (`dbNotify.c:196-270`): the put named `record`, so
    /// `dbChannelRecord(ppn->chan)` is `record` and that is the one record
    /// `dbNotifyDump` prints a block for.
    fn for_entry_record(record: &str, tx: crate::runtime::sync::oneshot::Sender<()>) -> Arc<Self> {
        Arc::new(Self {
            pending: AtomicUsize::new(1),
            tx: StdMutex::new(Some(tx)),
            entry: Some(record.into()),
        })
    }

    /// The record this notify was issued against, or `None` for a set with
    /// no `processNotify` behind it. See [`Self::entry`].
    pub(crate) fn entry_record(&self) -> Option<&str> {
        self.entry.as_deref()
    }

    /// A PP target joined the chain (C `dbNotifyAdd`). Balanced by exactly
    /// one [`Self::leave`].
    pub fn enter(&self) {
        self.pending.fetch_add(1, Ordering::AcqRel);
    }

    /// A record finished its contribution (C `dbNotifyCompletion`). Fires
    /// the completion oneshot on the `leave` that empties the set.
    pub fn leave(&self) {
        let prev = self.pending.fetch_sub(1, Ordering::AcqRel);
        debug_assert!(prev >= 1, "NotifyWaitSet::leave underflow");
        if prev == 1 {
            if let Some(tx) = self.tx.lock().unwrap().take() {
                let _ = tx.send(());
            }
        }
    }

    /// True once every chain member has left (the completion has fired).
    /// Used by the put entry to decide synchronous ([`ProcessCompletion::Sync`])
    /// vs async-pending ([`ProcessCompletion::Async`]) completion.
    pub fn completed(&self) -> bool {
        self.pending.load(Ordering::Acquire) == 0
    }
}

/// The completion outcome of an externally-initiated record process cycle —
/// the value a caller learns after driving the synchronous head of a
/// `dbPutNotify` / CA `WRITE_NOTIFY`.
///
/// This is the contract the **RTEMS CA driver** consumes: the CA thread drives
/// the synchronous head of a put (C `dbProcessNotify`, `rsrv/camessage.c`
/// `write_notify_action`) to completion — on RTEMS via `park_on` — then
/// `match`es this value to decide whether to reply inline or return now and let
/// background infrastructure deliver the completion later. The caller learns
/// sync-vs-async as a typed value, not by inferring it from `Option::is_some`.
///
/// # C parity (`dbNotify.c`)
///
/// The C `processNotify` state machine forks a put-notify exactly here:
///
/// * **[`Self::Sync`]** — the record was neither active (`pact`) nor selected
///   for processing, so `processNotifyCommon` runs `callDone`
///   (`dbNotify.c:270`), which fires `doneCallback` INLINE on the calling
///   thread (`dbNotify.c:182`). Our fully-synchronous chain drains the
///   [`NotifyWaitSet`] before the put entry returns.
/// * **[`Self::Async`]** — the record was `pact` (`notifyRestartInProgress`,
///   `dbNotify.c:225-231`) or processed into an async device
///   (`notifyProcessInProgress`, `dbNotify.c:252-263`). Completion is deferred
///   to `dbNotifyCompletion` (`dbNotify.c:445-475`), which fires the user
///   callback via `callbackRequest` (`:466`/`:470`) when the tracked waitList
///   empties. Our [`NotifyWaitSet::leave`]-to-zero fires the `handle` oneshot
///   at that same moment.
///
/// # Invariant (by construction)
///
/// Exactly one of {`Sync` returned, the `Async` handle fires exactly once} per
/// initiated cycle. The single owner of the fire is [`NotifyWaitSet`]: its
/// `leave`-to-zero `take`s the oneshot sender and sends once, so the handle can
/// never fire twice; and `Sync` is returned only when the wait-set already
/// drained, so no handle is outstanding to fire. There is no parallel
/// signalling path — the oneshot is the sole completion channel.
#[derive(Debug)]
pub enum ProcessCompletion {
    /// The cycle settled within the calling thread — the caller replies inline.
    Sync,
    /// The cycle went async; `handle` fires exactly once when the tracked
    /// FLNK/OUT chain settles (C `dbNotifyCompletion`).
    Async(crate::runtime::sync::oneshot::Receiver<()>),
}

impl ProcessCompletion {
    /// Build the outcome from the wait-set's internal signal. `None` — the
    /// wait-set drained synchronously, or the completion receiver lives
    /// elsewhere (a deferred-restart replay carries only the sender) — is
    /// [`Self::Sync`]; `Some(rx)` is [`Self::Async`].
    pub(crate) fn from_signal(rx: Option<crate::runtime::sync::oneshot::Receiver<()>>) -> Self {
        match rx {
            Some(rx) => Self::Async(rx),
            None => Self::Sync,
        }
    }

    /// The completion handle if this cycle went async, else `None`. The CA
    /// `WRITE_NOTIFY` dispatch uses this to choose inline reply (`None`) vs a
    /// spawned completion task (`Some(rx)`).
    pub fn into_handle(self) -> Option<crate::runtime::sync::oneshot::Receiver<()>> {
        match self {
            Self::Sync => None,
            Self::Async(rx) => Some(rx),
        }
    }

    /// True if the cycle went async (a completion handle is outstanding).
    pub fn is_async(&self) -> bool {
        matches!(self, Self::Async(_))
    }

    /// True if the cycle completed synchronously (no handle to await).
    pub fn is_sync(&self) -> bool {
        matches!(self, Self::Sync)
    }
}

/// A put-notify (`dbPutNotify` — CA WRITE_NOTIFY, `caput -c`) that landed on a
/// PACT record and was therefore deferred WHOLE.
///
/// C `processNotifyCommon` (dbNotify.c:225-231) tests `precord->pact` above
/// `ppn->putCallback`, so nothing is written and nothing is marked: the record
/// joins the notify's wait list in state `notifyRestartInProgress`, and when the
/// async cycle completes the put is replayed against a record that is no longer
/// active — value written, record processed, callback fired only after THAT
/// process finishes. So a client's "callback returned" still means "the value I
/// sent has been processed".
///
/// softIoc 7.0.10.1-DEV, `ASY` (calcout, `ODLY=4`, `A=5`), `caput -c ASY.A 7`
/// issued 1 s into the async cycle:
///
/// ```text
/// t=1s  A=5  PACT=1                      <- cycle in flight
/// t=2s  A=5  PACT=1  RPRO=0              <- put-notify pending: nothing written
/// t=4s  A=7  PACT=1                      <- cycle done; the put is replayed
/// callback returns at t=6.9s: A=7 VAL=7  <- after the RESTARTED process
/// ```
pub struct DeferredNotifyPut {
    /// The field the client wrote (already upper-cased).
    pub field: String,
    /// The value it wrote — held here, unwritten, until the restart.
    pub value: crate::types::EpicsValue,
    /// The client's completion channel. The replayed put builds its wait-set
    /// around this sender, so the callback fires on the restarted process, not
    /// on the in-flight cycle.
    pub completion: crate::runtime::sync::oneshot::Sender<()>,
}

/// One entry of C `precord->ppnr->restartList` — a whole `processNotify`
/// waiting for the record, not just a put.
///
/// C queues the *request* (`ellSafeAdd(&restartList, &ppn->restartNode)`,
/// dbNotify.c:217) and `restartCheck` re-enters `processNotifyCommon`, which
/// dispatches on `ppn->requestType`. A queue that could hold only a
/// field-and-value put left the other request type — C `processGetRequest`,
/// the port's [`crate::server::database::PvDatabase::process_record_with_notify`]
/// — with nowhere to wait, so its entry refused instead of queueing.
pub enum DeferredNotify {
    /// C `putProcessRequest` / `putProcessGetRequest`: write the field, then
    /// process; the callback fires on the replayed cycle.
    Put(DeferredNotifyPut),
    /// C `processGetRequest`: process the record, write nothing.
    Process {
        /// The client's completion channel, armed on the replayed process.
        completion: crate::runtime::sync::oneshot::Sender<()>,
    },
}

/// The PACT→idle transition, as a value.
///
/// Carries one bit: whether the record owed a restart at the moment the token
/// was minted. Queued put-notifies live on the record
/// (`RecordInstance::notify_restart_list`) from arrival to replay and are
/// promoted by one owner, `PvDatabase::apply_pact_exit` — so a release path
/// that forgets its tail delays a restart, it cannot strand one inside a
/// dropped value.
///
/// The bit is a HINT, not a second home for the queue. It is minted under a
/// record lock the minting site already holds (every constructor is reached
/// from `&mut self` or an explicit read), which is what lets
/// `apply_pact_exit` take NO record lock at all — so it is safe to call from
/// a `Drop`, where a still-live write guard in the same scope would otherwise
/// deadlock parking_lot. A stale `true` costs one no-op drain: the drain
/// re-reads the queue under the write lock via
/// `RecordInstance::take_next_notify_restart` and returns if it is empty.
///
/// The token is `#[must_use]` because that tail is where the restart happens:
/// C `recGbl.c:295` (`if (pdbc->ppn) dbNotifyCompletion(pdbc)`) →
/// `dbNotifyCompletion` → `restartCheck` (`dbNotify.c:149-170`). Holding it to
/// the tail rather than promoting at the `pact = FALSE` store is what keeps the
/// replay behind the rest of the cycle, exactly as C's queued callback is.
#[must_use = "a PACT release must reach PvDatabase::apply_pact_exit, which is \
              where a queued put-notify is restarted"]
pub struct PactExit {
    restart_pending: bool,
}

impl PactExit {
    /// Mint a token from the record's queue state, read under the caller's
    /// lock.
    pub(crate) fn new(restart_pending: bool) -> PactExit {
        PactExit { restart_pending }
    }

    /// Fold two releases of the same cycle into one token.
    ///
    /// A simulated SDLY continuation releases PACT inside `check_simulation_mode`
    /// and again at the `is_continuation` arm; one restart check covers both, so
    /// either half owing a restart makes the folded token owe one.
    pub(crate) fn merge(self, other: PactExit) -> PactExit {
        PactExit {
            restart_pending: self.restart_pending || other.restart_pending,
        }
    }

    /// Whether the minting site saw a queued notify. See the type docs: this
    /// is a hint the drain re-validates, never the queue itself.
    pub(crate) fn restart_pending(&self) -> bool {
        self.restart_pending
    }
}

/// Cached metadata for a record.
///
/// Stores the result of `populate_display_info` / `populate_control_info` /
/// `populate_enum_info` so subsequent `snapshot_for_field` /
/// `make_monitor_snapshot` calls can skip rebuilding the metadata. The
/// cache is invalidated whenever a metadata-class field is written
/// (EGU, PREC, HOPR, LOPR, alarm limits, DRVH/DRVL, state strings).
///
/// In a CA-only IOC this is a CPU win; in a hybrid CA + PVA IOC where
/// every snapshot needs full metadata for NTScalar serialization, the
/// cache eliminates redundant per-event populate work.
#[derive(Clone, Default)]
pub(crate) struct MetadataSnapshot {
    pub display: Option<DisplayInfo>,
    pub control: Option<ControlInfo>,
    pub enums: Option<EnumInfo>,
}

/// Does a write to this field make [`RecordInstance::metadata_cache`] stale?
///
/// **Cache bookkeeping only** — NOT the `DBE_PROPERTY` gate, which is the
/// field's own `prop(YES)` declaration ([`RecordInstance::field_posts_property`]).
/// The two used to be one hand-written list, so every field this port had to
/// invalidate on became a property event C does not post, and every `prop(YES)`
/// field nobody had listed posted nothing. They answer different questions and
/// the sets genuinely differ in both directions: `busy.ZNAM` is a cache source
/// that C does not mark `prop(YES)` (busy's `.dbd` declares no `prop` at all),
/// and `histogram.ULIM` is `prop(YES)` yet feeds only the live-computed
/// `apply_field_metadata_override`.
///
/// The rule: **every field read by `populate_display_info`,
/// `populate_control_info`, or `populate_enum_info` MUST be in this set** —
/// otherwise the cache serves stale metadata until some other source field is
/// written. Field name is expected uppercase.
///
/// `DESC` feeds `display.description` but is deliberately absent: its
/// invalidation is owned by the DESC arm of `put_common_field`, the single
/// writer of `common.desc`. The `Q:form` info tag (`populate_display_info` ->
/// `display.form`) is an immutable load-time tag, not a runtime field, so it
/// needs no invalidation either.
fn is_metadata_cache_source(name: &str) -> bool {
    matches!(
        name,
        // `populate_display_info` — units/precision/display limits for the
        // analog, integer, array and motor arms.
        "EGU" | "PREC" | "HOPR" | "LOPR" | "HLM" | "LLM"
        // `populate_control_info` — the ao/longout/int64out drive limits.
        | "DRVH" | "DRVL"
        // `populate_enum_info` via `Record::enum_state_strings` — bi/bo/busy
        // two-state names and the sixteen mbbi/mbbo state strings.
        | "ZNAM" | "ONAM"
        | "ZRST" | "ONST" | "TWST" | "THST" | "FRST" | "FVST" | "SXST" | "SVST"
        | "EIST" | "NIST" | "TEST" | "ELST" | "TVST" | "TTST" | "FTST" | "FFST"
    )
}

/// One alarm limit for a DBR_AL_DOUBLE response: the value when its
/// severity threshold is enabled, `NaN` otherwise. Mirrors C
/// `get_alarm_double`'s `prec->hhsv ? prec->hihi : epicsNAN` — a NONZERO
/// test on the raw ordinal, so an out-of-range severity still enables the
/// limit.
fn gated(severity: i16, limit: f64) -> f64 {
    if severity != 0 { limit } else { f64::NAN }
}

/// Extract the RAW stored ordinal a put lands in a `menu(menuAlarmSevr)`
/// severity field (`HHSV`/`HSV`/`LSV`/`LLSV`/`UDFS`/`DISS`), WITHOUT clamping
/// to the 0..=3 valid range.
///
/// C's numeric menu put stores whatever `(epicsEnum16)` the value truncates to
/// (`dbConvert.c::putDoubleEnum` = `*pfield = (epicsEnum16)*psrc`), so
/// `caput REC.HSV 4` keeps `4` and `caput REC.HSV -1` keeps `65535` — both
/// wire-visible (served signed as `-1`) and both used verbatim to derive the
/// alarm. The carrier is `i16` so the 16-bit pattern round-trips; the alarm
/// meaning is read back with [`AlarmSeverity::from_u16`] and the C nonzero
/// enable with `!= 0`.
///
/// A numeric value has already been wrapped to `epicsEnum16` upstream
/// (`EpicsValue::convert_to(Enum)`, the one owner of C's double→enum cast); this
/// only reinterprets its bit pattern. A `String` is a db-load / internal-link
/// label (a client string put is rejected-or-resolved by `putStringMenu`
/// upstream), resolved to its ordinal here.
fn menu_ordinal_raw(value: &EpicsValue) -> i16 {
    match value {
        EpicsValue::String(s) => match s.as_str_lossy().as_ref() {
            "NO_ALARM" => 0,
            "MINOR" => 1,
            "MAJOR" => 2,
            "INVALID" => 3,
            other => other
                .parse::<i64>()
                .ok()
                .map(|n| n as u16 as i16)
                .unwrap_or(0),
        },
        other => other.to_f64().unwrap_or(0.0) as i64 as u16 as i16,
    }
}

/// Coerce a db-loaded `String` for a numeric/menu **common** field to that
/// field's canonical DBF type before [`RecordInstance::put_common_field`]
/// dispatches on it.
///
/// The db loader applies a record's own fields with the typed
/// `EpicsValue::parse(desc.dbf_type, value_str)` (`db_loader::apply_fields`),
/// but a field absent from `field_list` is pushed to the common-field path as
/// a raw `EpicsValue::String` — it has no `FieldDesc` to parse against. The
/// numeric common-field arms in `put_common_field` match only their typed
/// variant, so without this step a `.db` `field(PHAS, "1")`,
/// `field(PRIO, "HIGH")`, `field(DISS, "MAJOR")`, `field(DISA, "1")`, … is
/// silently dropped at IOC load. Routing the String through the same
/// `EpicsValue::parse` the record-field path uses handles the numeric *and*
/// menu-label forms uniformly, so the arm receives the value it expects.
///
/// Only fields whose canonical type is numeric/menu are listed; the
/// Port of libcom `epicsParseInt32(str, &to, 10, NULL)`
/// (`libcom/src/misc/epicsStdlib.c:26-53,245-261`), which is how pvxs parses
/// the `nsec:lsb:` digit count. Returns `None` for every status the C
/// returns non-zero for:
///
/// - `S_stdlib_noConversion` — `strtol` consumed nothing (empty / no digits)
/// - `S_stdlib_extraneous` — trailing non-space bytes with `units == NULL`
/// - `S_stdlib_overflow` — outside `epicsInt32`
///
/// Leading and trailing whitespace — C `isspace`, vertical tab included — and
/// a leading `+`/`-` sign are accepted, matching `epicsParseLong`'s skips and
/// `strtol`.
fn epics_parse_int32_base10(s: &str) -> Option<i32> {
    // `while ((c = *str) && isspace(c)) ++str;` then `strtol(str, &endp, 10)`.
    let body = s.trim_start_matches(crate::runtime::stdlib::c_isspace);
    let (sign, digits) = match body.strip_prefix(['+', '-']) {
        Some(rest) if body.starts_with('-') => (-1i64, rest),
        Some(rest) => (1i64, rest),
        None => (1i64, body),
    };
    let end = digits
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(digits.len());
    if end == 0 {
        return None; // endp == str → S_stdlib_noConversion
    }
    // `if (c && !units) return S_stdlib_extraneous;` after skipping trailing
    // whitespace.
    if !digits[end..]
        .trim_start_matches(crate::runtime::stdlib::c_isspace)
        .is_empty()
    {
        return None;
    }
    // ERANGE from `strtol`, then the explicit `epicsInt32` range check.
    let magnitude: i64 = digits[..end].parse().ok()?;
    i32::try_from(sign * magnitude).ok()
}

/// The STORED type of a `dbCommon` field — the variant its
/// [`RecordInstance::put_common_field_bounded`] arm binds, which is not always
/// the type the `.dbd` DECLARES it as (a `menu()` field is declared `DBF_MENU`
/// and served `DBR_ENUM`, but held here as its bare index).
///
/// String-typed common fields (DESC, ASG, OUT, TSEL, …) have no entry: their
/// arms take the string verbatim.
fn stored_common_field_type(name: &str, declared: Option<DbFieldType>) -> Option<DbFieldType> {
    Some(match name {
        "SCAN" | "SSCN" | "PINI" => DbFieldType::Enum,
        "TSE" | "PHAS" | "PRIO" | "DISV" | "DISA" | "DISS" | "LCNT" | "UDFS" | "ACKT" | "ACKS"
        | "SEVR" | "STAT" | "NSEV" | "NSTA" => DbFieldType::Short,
        // The analog-alarm limits and the hysteresis margin are the one row
        // here whose stored type is the DECLARED type, and it varies by record:
        // `DBF_DOUBLE` on ai/ao/calc/calcout/sub/scalcout, `DBF_LONG` on
        // longin/longout, `DBF_INT64` on int64in/int64out
        // (`int64inRecord.dbd.pod:152-208`). Naming `Double` for all of them
        // discarded the record's own `.dbd` row on both writers — the `.db`
        // string parse and a runtime `dbPut` — so an `epicsInt64` limit above
        // 2^53 was rounded before it ever reached storage. Ask the record's
        // generated field table instead; a caller with no table (a hand-built
        // test record) keeps the `DBF_DOUBLE` majority.
        //
        // A field missing from this table entirely reaches its arm as whatever
        // variant the caller built, and an arm that binds a typed variant then
        // drops it: that is how `field(HYST,"2")` silently became 0 on every
        // record whose hysteresis lives in `common.hyst`.
        "HIHI" | "HIGH" | "LOW" | "LOLO" | "HYST" => declared.unwrap_or(DbFieldType::Double),
        // The `DBF_UCHAR` flags. `bool` here, a NUMBER in C.
        "DISP" | "UDF" | "TPRO" | "RPRO" | "BKPT" | "PROC" => DbFieldType::Char,
        _ => return None,
    })
}

/// **The single owner of "what type does a `dbCommon` field hold"**, run on
/// EVERY put before the typed arms below see the value — so an arm may bind one
/// variant and know the put cannot have arrived in another.
///
/// This is not a string-parsing convenience. A common field is reached by three
/// writers with three different ideas of the value's shape: the db loader hands
/// every field over as a raw `String`; a `dbPut` arrives coerced to the field's
/// DECLARED type (`DBF_MENU` → `Enum` for PRIO, `DBF_UCHAR` → `Char` for DISP);
/// an internal link delivers whatever its source stored. Before this ran on the
/// non-`String` shapes too, each arm's single-variant `if let` was a silent
/// drop for the other two writers — `caput REC.PRIO HIGH` resolved its label to
/// `Enum(2)` and then vanished at the arm, leaving PRIO at 0.
///
/// An unparseable String is returned as-is so the arm drops it, and a menu
/// field's bad label FAILS the put (`S_db_badChoice`) rather than landing as
/// index 0.
fn coerce_common_field(
    name: &str,
    value: EpicsValue,
    bound: MenuBound,
    declared: Option<DbFieldType>,
) -> CaResult<Converted> {
    let Some(dbf) = stored_common_field_type(name, declared) else {
        return Ok(Converted::Stored(value));
    };
    let EpicsValue::String(s) = &value else {
        // Already typed: project onto the stored type through the one
        // value-coercion owner. `convert_to` short-circuits a value that is
        // already `dbf`, so the common case costs nothing.
        return Ok(Converted::Stored(value.convert_to(dbf)));
    };
    let text = s.as_str_lossy();
    // A `DBF_MENU` common field resolves its label against THAT field's own
    // menu through the one converter every menu-field string put uses
    // (C `dbConvert.c::putStringMenu`: exact label, else an index below
    // `nChoice`, else `S_db_badChoice`) — the same rule the record-specific
    // menu fields follow in `coerce_write_value`. The failure PROPAGATES: the
    // field-blind `EpicsValue::parse` fallback below must never see a menu
    // field, or `caput REC.PRIO Bogus` lands as index 0 instead of failing.
    //
    // SCAN/SSCN/PINI are menu fields like any other and go through the same
    // converter. They used to each carry a hand-written `from_str` that drifted
    // from C: `ScanType::from_str` case-folded and invented `"0.5 second"`
    // aliases for menuScan's `".5 second"` (and mapped any out-of-range index
    // to Passive), `SimModeScan::from_str` took any u16, `PiniMode::from_str`
    // trimmed. C has ONE converter and it does none of that.
    if let Some(choices) = super::menu_choices::shared_menu_choices(name) {
        return super::menu_choices::resolve_menu_field_string_bounded(
            name, choices, dbf, &text, bound,
        )
        .map(Converted::Stored);
    }
    // Numeric (non-menu) common field: C's `dbPut` runs the string through the
    // SAME `epicsParse*` (`dbConvert.c` `putString*`) the record data fields use,
    // and a non-zero status REFUSES the whole put (`dbAccess.c:1362`, mapped to
    // `ECA_PUTFAIL`). Route it through the single owner of that conversion —
    // [`c_parse::put_string`] — instead of the field-blind `EpicsValue::parse`,
    // which wrapped (`256 as u8 == 0`) and swallowed the error (`Err(_) =>
    // Ok(value)`), so `caput REC.PROC 256` and `caput REC.PROC notanumber` were
    // accepted where C rejects them.
    //
    // Key the parse on the field's C-DECLARED width, not its stored variant: the
    // `DBF_UCHAR` flags (DISP/UDF/TPRO/RPRO/BKPT/PROC, `dbCommon.dbd`) are held in
    // the signed `Char` variant here but C parses them with `epicsParseUInt8`, so
    // `caput REC.PROC 255` and `caput REC.PROC -1` (→255) are accepted and only
    // `256`+/non-numeric refused. `put_string` returns the value in the declared
    // variant; project it back onto the stored variant through the one
    // value-coercion owner (byte-identity for `UChar`→`Char`).
    let declared = match dbf {
        DbFieldType::Char => DbFieldType::UChar,
        other => other,
    };
    let Some(target) = c_parse::NumericField::of(declared) else {
        // Unreachable: every numeric `stored_common_field_type` (Char/Short/
        // Double, all with a numeric row) reaches here; the `Enum` menu types
        // returned above. Keep the pre-parse value rather than panic.
        return Ok(Converted::Stored(value));
    };
    // Hand the string over UNTRIMMED. C tests `*from == 0` on the raw bytes
    // (`dbFastLinkConv.c:147`), so `"   "` is not the empty string to it and
    // falls through to `epicsParse*`, which refuses it; trimming first turned
    // it into the accepted empty case. Nothing else was riding on the trim —
    // `scan_int`/`strtod` skip leading `isspace` themselves, and `epicsParse*`
    // is called with a non-NULL `units` pointer, so trailing text is legal.
    Ok(match c_parse::put_string(name, target, &text)? {
        c_parse::Converted::Stored(parsed) => Converted::Stored(parsed.convert_to(dbf)),
        c_parse::Converted::Unchanged => Converted::Unchanged,
    })
}

/// The alarm-acknowledge request types C's `dbPut` dispatches on
/// (`dbAccess.c:1331-1335`): `DBR_PUT_ACKT` and `DBR_PUT_ACKS`.
///
/// Acknowledgement is a *request type*, not a field write — the two handlers
/// run above the `SPC_NOMOD` gate that refuses every ordinary put to ACKT/ACKS.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AlarmAck {
    /// `DBR_PUT_ACKT` → `putAckt`: set transient-alarm acknowledgement.
    Transient,
    /// `DBR_PUT_ACKS` → `putAcks`: acknowledge an alarm of this severity.
    Severity,
}

/// A type-erased record instance stored in the database.
pub struct RecordInstance {
    pub name: String,
    pub record: Box<dyn Record>,
    pub common: CommonFields,
    pub subscribers: HashMap<String, Vec<Subscriber>>,
    /// Terminal destruction marker, the [`crate::server::pv::ProcessVariable`]
    /// flag's counterpart for a record-backed channel. Set once by
    /// [`Self::destroy`], whose only caller is
    /// [`crate::server::database::PvDatabase::remove_record`], so *removed
    /// from the database* and *destroyed* are one event for both target
    /// kinds and a server can sweep them with one uniform test.
    destroyed: bool,
    // Link parse cache
    pub parsed_inp: ParsedLink,
    pub parsed_out: ParsedLink,
    pub parsed_flnk: ParsedLink,
    pub parsed_sdis: ParsedLink,
    pub parsed_tsel: ParsedLink,
    // Device support
    pub device: Option<Box<dyn super::super::device_support::DeviceSupport>>,
    // Subroutine (for sub records)
    pub subroutine: Option<Arc<SubroutineFn>>,
    /// PACT (C `precord->pact`) — the re-entrancy guard, and the record's
    /// "busy" state for every put that lands on it.
    ///
    /// PRIVATE by construction: entered through [`RecordInstance::enter_pact`]
    /// and released ONLY through [`RecordInstance::leave_pact`], which hands
    /// back the [`PactExit`] that routes the release to the cycle tail where
    /// queued put-notifies are restarted. A `pact.store(false)` open-coded at
    /// a release site is what skipped that tail on the ODLY/SDLY paths; it is
    /// no longer expressible.
    pact: AtomicBool,
    // Put-notify wait-set this record currently belongs to (C
    // `precord->ppn`). Set when the record joins an active put-notify
    // (originating put target, or a FLNK/OUT PP target via `dbNotifyAdd`);
    // taken + `leave`d when the record's processing completes. `None`
    // outside any put-notify. See [`NotifyWaitSet`].
    // Private to the crate: the two writers ([`RecordInstance::
    // install_or_queue_notify`] and [`RecordInstance::join_put_notify`]) are
    // the slot's only assignment sites, and a `pub` field made a third one
    // constructible from outside. Read it with [`RecordInstance::has_notify`].
    pub(crate) notify: Option<Arc<NotifyWaitSet>>,
    /// C `precord->ppnr->restartList` — put-notifies waiting to take this
    /// record, oldest first.
    ///
    /// `processNotifyCommon` (dbNotify.c:213-219, 225-231) tests both
    /// "another processNotify owns the record" and `precord->pact` ABOVE
    /// `putCallback`, so a `dbPutNotify` onto a busy record writes nothing:
    /// no value, no RPRO. The whole put — value, process, callback — is
    /// deferred and restarted later. C queues them with `ellSafeAdd` and
    /// promotes one per completion (`restartCheck`, dbNotify.c:149-170); this
    /// is that list, and modelling it as a list rather than one slot is what
    /// stops the second concurrent `caput -c` being refused with an
    /// `ECA_PUTCBINPROG` C never sends.
    ///
    /// PRIVATE. Appended only by [`Self::queue_notify_put`], drained only by
    /// [`Self::take_next_notify_restart`], which pops only onto a record no
    /// put-notify owns.
    notify_restart_list: std::collections::VecDeque<DeferredNotify>,
    /// The value of each subscribed field as ALREADY PUBLISHED to that
    /// field's `DBE_VALUE`/`DBE_LOG` subscribers. The generic
    /// change-detection loop in every snapshot builder posts a field only
    /// when its current value differs from this — so this map is what
    /// C's per-record `*_lst` / MARK state is to `monitor()`.
    ///
    /// # Invariant (CONTRACT)
    ///
    /// A field's value MUST NOT be published twice by the framework.
    /// Concretely: every value-class post (a `db_post_events` carrying
    /// `DBE_VALUE` and/or `DBE_LOG`) MUST advance this map for the field it
    /// posts; an alarm-only / property-only post MUST NOT (those classes do
    /// not deliver the value to a `DBE_VALUE`/`DBE_LOG` subscriber, so the
    /// change is still owed to them).
    ///
    /// In C, `dbPut` (dbAccess.c:1407-1414) is the record's ONLY post for a
    /// put: `db_post_events(precord, pfieldsave, DBE_VALUE|DBE_LOG)`. No
    /// record's `monitor()` re-posts that field — it posts a closed set and
    /// compares against its own `*_lst` fields. A framework that posts on the
    /// put and then change-detects the same field on the next process cycle
    /// sends an event C never sends.
    ///
    /// # Owner
    ///
    /// [`RecordInstance::record_value_post`] is the SINGLE writer. The field
    /// is private so no path outside this module can advance (or fail to
    /// advance) it: the snapshot builders read it through
    /// [`RecordInstance::posted_value`] and every poster —
    /// [`RecordInstance::notify_field_with_origin`] included — advances it
    /// through the owner.
    last_posted: HashMap<String, EpicsValue>,
    /// The live store for a field the record's `.dbd` DECLARES but the record
    /// struct has no `put_field` arm / no storage for — the WRITE analog of the
    /// read-side [`Self::declared_default`] fallback.
    ///
    /// C makes every `.dbd` field not just readable but WRITABLE: `dbPutField`
    /// resolves the field from its `dbFldDes` and `dbPut` writes the incoming
    /// value into record memory, whether or not any record code ever reads it
    /// back — a `caput dfanout.HOPR 10` sticks even though `dfanoutRecord.c`
    /// never touches HOPR. A Rust record models only the fields it has
    /// behaviour for, so a field it declares but never stores had nowhere for a
    /// put to land: [`Self::put_common_field`]'s catch-all reported
    /// `S_dbLib_fieldNotFound` and the client's put was refused, while a READ of
    /// the same field succeeded through `declared_default`. This map is that
    /// missing storage — one uniform mechanism for the whole family, not a
    /// per-field struct member on each record type.
    ///
    /// Keyed by upper-case field name, holding the value already coerced to the
    /// field's C-declared DBF type (the same projection `declared_default` and
    /// the read path serve). [`Self::resolve_field`] reads it BEFORE
    /// `declared_default`, so a read reflects a prior write and an untouched
    /// field still reads its `.dbd` initial. Empty for a record whose declared
    /// fields are all modeled.
    declared_overrides: HashMap<String, EpicsValue>,
    /// This record's OWN link fields whose target supplies some field's
    /// units/precision/graphic/alarm — the distinct answers of
    /// [`Record::link_backed_metadata_field`] over the record's declared
    /// field list, collected ONCE here.
    ///
    /// Derived, never declared a second time: the record type states the
    /// mapping in one place and this is the reverse index of that one
    /// statement, so the two cannot drift the way the central
    /// `match rtype` list they replace drifted away from `aSub`.
    link_backed_metadata_links: Vec<String>,
    /// Set by `check_deadband_ext` for waveform/aai/aao when their
    /// content hash changed this cycle (C `monitor()` On Change mode,
    /// waveformRecord.c:310-319). The snapshot builders read it to post
    /// `HASH` with a literal `DBE_VALUE` event, independent of the VAL
    /// post mask. False for every record without the MPST/APST/HASH
    /// mechanism.
    pub(crate) array_hash_changed: bool,
    /// One-shot "skip the registered subroutine this cycle" signal for aSub
    /// `LFLG=READ`. The async processing path resolves the `SUBL` link before
    /// taking this lock; when the resolved name is bad (C `fetch_values` ->
    /// `S_db_BadSub`) or the link read failed, C `process` runs `do_sub` only
    /// on `!status`, so the subroutine is skipped. Set by the resolution
    /// apply, consumed (and cleared) by [`Self::run_registered_subroutine`];
    /// `false` for every record without a pending bad re-resolution.
    pub(crate) suppress_subroutine_run: bool,
    /// Generation counter for ReprocessAfter timer cancellation.
    /// Bumped each process cycle. Spawned timers check this to avoid
    /// stale re-processes from accumulated timers.
    pub reprocess_generation: Arc<std::sync::atomic::AtomicU64>,
    /// Generation counter for the monitor watchdog
    /// ([`Record::watchdog_interval`] / [`Record::watchdog_fire`]), bumped by
    /// each `PvDatabase::arm_watchdog` so a re-arm supersedes the tick already
    /// in flight — C `callbackRequestDelayed` replacing an outstanding delayed
    /// callback. Deliberately NOT `reprocess_generation`: C's histogram wdog is
    /// its own `epicsCallback`, independent of the record's SDLY/async
    /// re-entry, so an SDLY defer must not cancel the watchdog nor vice versa.
    pub watchdog_generation: Arc<std::sync::atomic::AtomicU64>,
    /// Per-record info tags from `info("key", "value")` directives in
    /// the .db file (epics-base info(...) grammar). Consumers include
    /// asyn (`asyn:READBACK`), record-as-PV bridge tags
    /// (`Q:group`, `Q:form`), and IOC-specific extensions. Empty for
    /// records loaded without info(...) clauses.
    pub info: HashMap<String, String>,
    /// Cached metadata (display/control/enums) — `None` means stale or
    /// not yet built. Populated lazily by `snapshot_for_field` /
    /// `make_monitor_snapshot` and invalidated by `invalidate_metadata_cache`
    /// whenever a metadata-class field (EGU/PREC/HOPR/LOPR/limit/state)
    /// is written.
    ///
    /// Wrapped in `std::sync::Mutex` for interior mutability — the
    /// containing `RecordInstance` is shared via `Arc<RwLock<...>>` from
    /// `PvDatabase`, and snapshot construction holds a read lock; the
    /// inner Mutex lets us still mutate the cache from a `&self` method.
    ///
    /// # Cache invariant (CONTRACT)
    ///
    /// The cache is **only correct under the following contract**: every
    /// code path that mutates a cache-source field (the set defined in
    /// the file-private [`is_metadata_cache_source`] predicate) MUST call
    /// [`RecordInstance::notify_field_written`] (or
    /// [`RecordInstance::invalidate_metadata_cache`] directly) afterward.
    ///
    /// All current write paths in `field_io.rs` already do this. If you
    /// add a new code path that:
    ///
    /// - calls `instance.record.put_field(...)` directly, OR
    /// - mutates record fields from inside `Record::process()`,
    ///   `Record::on_put`, or `Record::special` and that mutation could
    ///   touch a cache-source field, OR
    /// - lets a `Box<dyn Record>` implementation expose its own
    ///   mutation methods that change cache-source fields,
    ///
    /// then call `instance.notify_field_written(field_name)` to keep the
    /// cache consistent. Forgetting will produce a stale snapshot —
    /// monitors will continue to see the old EGU/PREC/limits until the
    /// next legitimate cache-source write triggers invalidation.
    ///
    /// # Symmetric note for `populate_*` extensions
    ///
    /// If a future change adds a new field to `populate_display_info`,
    /// `populate_control_info`, or `populate_enum_info`, the new source
    /// field name MUST also be added to [`is_metadata_cache_source`] so
    /// writes to it invalidate the cache — unless, like DESC
    /// (`display.description`), its write owner invalidates directly (see
    /// the DESC arm of `put_common_field`). This set says nothing about
    /// `DBE_PROPERTY`, which the field's own `prop(YES)` declaration
    /// decides ([`RecordInstance::field_posts_property`]). (The `Q:form`
    /// -> `display.form` mapping is exempt: it reads an immutable
    /// load-time info tag, not a runtime field.)
    pub(crate) metadata_cache: StdMutex<Option<MetadataSnapshot>>,
}

/// The cycle status [`RecordInstance::run_registered_subroutine`] reports when
/// `do_sub` was skipped — C's `fetch_values` failure / `S_db_BadSub` path, which
/// leaves `process`'s `status` non-zero (aSubRecord.c:216-224).
const SUBROUTINE_STATUS_SKIPPED: i64 = -1;
/// Which C `do_sub` a record type owns. Only `subRecord.c` and `aSubRecord.c`
/// define one; every other record type reaches
/// [`RecordInstance::run_registered_subroutine`] with no subroutine bound for
/// the trivial reason that it never had one, and must not be handed `do_sub`'s
/// bad-sub verdict. Resolving the kind once also keeps the two record types'
/// three points of divergence (empty-SNAM exemption, bad-sub status, UDF
/// clear) reading off one decision instead of three `record_type()` compares.
#[derive(Clone, Copy, PartialEq, Eq)]
enum SubroutineKind {
    /// `subRecord.c::do_sub` — VAL is the subroutine's computed value.
    Sub,
    /// `aSubRecord.c::do_sub` — VAL is the returned status.
    ASub,
}

impl SubroutineKind {
    fn of(record_type: &str) -> Option<Self> {
        match record_type {
            "sub" => Some(Self::Sub),
            "aSub" => Some(Self::ASub),
            _ => None,
        }
    }
}

/// C `S_db_BadSub` — `(M_dbAccess | 35)` with `M_dbAccess = 511 << 16`
/// (`dbAccessDefs.h:189`, `errMdef.h:39`), i.e. 33488931. aSub's `do_sub`
/// returns it verbatim for an unregistered SNAM and `process` publishes it as
/// VAL, so the number is observable on the wire and cannot be a private
/// sentinel.
const S_DB_BAD_SUB: i64 = (511 << 16) | 35;
/// The bound subroutine returned `Err` — no C counterpart (a C subroutine
/// returns a `long`), and a failed cycle either way.
const SUBROUTINE_STATUS_ERROR: i64 = -3;

/// C `monitor()`'s post of the deadband field, as assembled by the single owner
/// [`RecordInstance::deadband_post`].
pub(crate) struct DeadbandPost {
    /// C's `monitor_mask` for this cycle. Also the mask the
    /// [`Record::fields_posted_with_value_mask`] secondaries ride: C posts them
    /// from INSIDE the `if (monitor_mask)` guard, with the same mask.
    pub mask: EventMask,
    /// The deadband field's own post — `(field, value)`. `None` when no class
    /// fired (C's `if (monitor_mask)` skips the post) or the field does not
    /// resolve.
    pub field: Option<(String, EpicsValue)>,
}

/// A value's `DBR_STRING` form, for a source whose field metadata is NOT
/// reachable (an external CA/PVA link, a constant, an lnkCalc result) — the
/// fallback half of [`RecordInstance::field_as_dbr_string`], which is also the
/// whole rule once the local choice table has had its say.
///
/// A pvalink NTEnum still resolves its label here: the carrier brings its own
/// `choices` (pvxs `pvxs/ioc/pvalink_lset.cpp:344-356` — a `DBR_STRING` target copies
/// `choices[index]`). A bare `Enum` index from a link whose labels the port
/// cannot reach falls back to its decimal form, like the CA `*_STRING` encoder.
/// **The** declaration lookup: a field's `dbFldDes`, in C's terms.
///
/// The record type's declaration is [`FieldDeclaration::field_list`] — the
/// generated `.dbd` table where one exists and the hand-written table where it
/// does not, never both. `dbCommon` is asked last, so a record-specific field
/// shadows the common one.
///
/// A free function, not just a [`RecordInstance`] method, because the sites that
/// need the declaration do not all hold an instance — a constant-link seed, a
/// link write, the db loader all have `&dyn Record`.
///
/// `None` for a field with no declaration at all: a virtual field (`RTYP`,
/// `TIME`, ...), which C answers from dbStaticLib rather than from a `dbFldDes`.
pub(crate) fn field_desc_of<R: Record + ?Sized>(
    record: &R,
    field: &str,
) -> Option<&'static FieldDesc> {
    let named = |t: &'static [FieldDesc]| t.iter().find(|f| f.name.eq_ignore_ascii_case(field));
    named(record.field_list()).or_else(|| named(super::dbd_generated::DB_COMMON_FIELDS))
}

/// **The single owner of "which choice list does this field resolve against"**,
/// asked by BOTH sides of a menu field:
///
/// * the READ side ([`RecordInstance::enum_string_form_for`]) — C `getMenuString`,
///   which renders the stored index as its choice;
/// * the WRITE side ([`crate::server::record::coerce_put_value`]) — C
///   `putStringMenu`, which resolves an incoming label to that same index.
///
/// They MUST see the same list or the field is not round-trippable. The write
/// side used to ask only [`Record::menu_field_choices`] (the record's hand
/// table), so an `aSub`'s `caput FTA LONG` found no menu, fell through to the
/// numeric parse and landed as index 0 — while the read side, on the `.dbd`
/// menu, rendered index 0 as `STRING`.
///
/// Order is C's: the field's own `menu()` from the declaration, then the
/// record's hand table where the `.dbd` does not reach (the downstream crates'
/// record types), then the `dbCommon` menus.
///
/// The last step — [`shared_menu_choices`](super::menu_choices::shared_menu_choices)
/// — is a heuristic keyed on the field NAME (`OSV`, `SIMS`, `HHSV`, …), so it is
/// consulted ONLY when the field's declaration does not already pin a non-menu
/// type: a menu is served as `DBR_ENUM`, so a field DECLARED `DBF_STRING` is
/// never a menu. Without this gate scalcout's string `OSV` ("Output string
/// value") matched the name-based `menuAlarmSevr` entry a same-named bi/bo
/// severity field owns, and `caput SCALCOUT.OSV <string>` was rejected with
/// `S_db_badChoice` where C accepts the string.
pub(crate) fn menu_choices_of<R: Record + ?Sized>(
    record: &R,
    field: &str,
) -> Option<&'static [&'static str]> {
    // `DTYP` is `DBF_DEVICE`: its choices are the record type's DEVICE menu
    // (C `dbDeviceMenu`, built from the `device()` declarations), which is
    // per-record-type and so cannot live in the shared `dbCommon` FieldDesc.
    if field.eq_ignore_ascii_case("DTYP") {
        return super::dbd_generated::device_menu(record.record_type());
    }
    let desc = field_desc_of(record, field);
    desc.and_then(|f| f.menu)
        .or_else(|| record.menu_field_choices(field))
        .or_else(|| {
            // Name-based fallback — but the declared type wins: a `DBF_STRING`
            // field is not a menu even when a same-named field elsewhere is.
            if desc.is_some_and(|f| f.dbf_type == DbFieldType::String) {
                None
            } else {
                super::menu_choices::shared_menu_choices(field)
            }
        })
}

/// The `DBF_*` type `field` is SERVED as, for a caller that holds only a
/// `&dyn Record` — the free-function form of
/// [`RecordInstance::declared_field_type`], and the ONLY way any site outside
/// the instance may turn a [`FieldDesc`] into a type.
///
/// A [`FieldDesc::runtime_typed`] field (`waveform.VAL` typed by `FTVL`, an
/// `aSub`'s `A`..`U` typed by `FTA`..`FTU`) has NO type in its declaration: C's
/// `cvt_dbaddr` overwrites `paddr->field_type` from record state, so the `.dbd`
/// entry is a placeholder and the value the record stores is the answer. Every
/// caller falls back to that value, which is why this returns `None` rather than
/// the placeholder — handing out `DBF_DOUBLE` for a `FTVL=CHAR` waveform is how
/// a string written down an output link became `0.0`.
pub(crate) fn declared_field_type_of<R: Record + ?Sized>(
    record: &R,
    field: &str,
) -> Option<DbFieldType> {
    let desc = field_desc_of(record, field)?;
    (!desc.runtime_typed).then_some(desc.dbf_type)
}

pub(crate) fn value_as_dbr_string(value: &EpicsValue) -> Option<PvString> {
    match value {
        EpicsValue::String(s) => Some(s.clone()),
        EpicsValue::Enum(v) => Some(PvString::from(v.to_string())),
        EpicsValue::EnumWithChoices { index, choices } => Some(
            choices
                .get(*index as usize)
                .cloned()
                .unwrap_or_else(|| PvString::from(index.to_string())),
        ),
        other => match other.clone().convert_to(DbFieldType::String) {
            EpicsValue::String(s) => Some(s),
            _ => None,
        },
    }
}

/// The `dbCommon` link fields, each with the C link-field type its text is
/// parsed under (`dbStaticLib.c:2380-2391`): `INP`/`TSEL`/`SDIS` are
/// `DBF_INLINK`, `OUT` is `DBF_OUTLINK`, `FLNK` is `DBF_FWDLINK`.
///
/// These five — and only these five — have a parse cache on
/// [`RecordInstance`], which is what lets a one-shot init decision (C
/// `dbInitLink` setting `DBLINK_FLAG_INITIALIZED`) be committed for them.
/// The list has one owner because it is read from three places that must not
/// drift: the per-field parse in `put_common_field`, the database's
/// `record_link_fields` enumeration, and the `initialize_link_locality`
/// commit. `FLNK` missing from just one of them is exactly how an external
/// forward link went un-opened at init.
pub const COMMON_LINK_FIELDS: [(&str, super::link::LinkFieldType); 5] = [
    ("INP", super::link::LinkFieldType::In),
    ("OUT", super::link::LinkFieldType::Out),
    ("TSEL", super::link::LinkFieldType::In),
    ("SDIS", super::link::LinkFieldType::In),
    ("FLNK", super::link::LinkFieldType::Fwd),
];

impl RecordInstance {
    pub fn new(name: String, record: impl Record) -> Self {
        Self::new_boxed(name, Box::new(record))
    }

    /// The raw text of one `COMMON_LINK_FIELDS` entry, or `None` for any
    /// other field name.
    pub fn common_link_text(&self, field: &str) -> Option<&str> {
        Some(match field {
            "INP" => self.common.inp.as_str(),
            "OUT" => self.common.out.as_str(),
            "TSEL" => self.common.tsel.as_str(),
            "SDIS" => self.common.sdis.as_str(),
            "FLNK" => self.common.flnk.as_str(),
            _ => return None,
        })
    }

    /// The parse cache of one `COMMON_LINK_FIELDS` entry, or `None` for any
    /// other field name. The only mutable handle on the cache outside
    /// `put_common_field`, so the iocInit locality commit cannot reach a slot
    /// that has no matching raw text.
    pub fn common_link_cache_mut(&mut self, field: &str) -> Option<&mut ParsedLink> {
        Some(match field {
            "INP" => &mut self.parsed_inp,
            "OUT" => &mut self.parsed_out,
            "TSEL" => &mut self.parsed_tsel,
            "SDIS" => &mut self.parsed_sdis,
            "FLNK" => &mut self.parsed_flnk,
            _ => return None,
        })
    }

    /// The link fields whose target metadata this record's rset serves — the
    /// work list [`PvDatabase::resolve_link_backed_metadata`] resolves for a
    /// batch post, and the set `Self::link_backed_metadata_field_of` answers
    /// one field out of.
    ///
    /// [`PvDatabase::resolve_link_backed_metadata`]: crate::server::database::PvDatabase
    pub fn link_backed_metadata_links(&self) -> &[String] {
        &self.link_backed_metadata_links
    }

    pub fn new_boxed(name: String, record: Box<dyn Record>) -> Self {
        let rtype = record.record_type();
        // The reverse index of `Record::link_backed_metadata_field`, built once
        // from the record's own declaration so no second list can go stale.
        // Empty for every record type that answers `None` — which is all but
        // calc, calcout, sub, seq and aSub.
        let link_backed_metadata_links: Vec<String> = {
            use crate::server::record::FieldDeclaration;
            let mut links: Vec<String> = record
                .field_list()
                .iter()
                .filter_map(|d| record.link_backed_metadata_field(d.name))
                .collect();
            links.sort_unstable();
            links.dedup();
            links
        };
        let analog_alarm = match rtype {
            // C parity: every record type whose dbd carries
            // HIHI/HIGH/LOW/LOLO/HHSV/HSV/LSV/LLSV gets an analog-alarm
            // config slot. Previously calc / calcout were missing —
            // their put_field for those fields silently no-op'd
            // because `self.common.analog_alarm` was None at the
            // mutation site. Confirmed via
            // calcRecord.dbd.pod:716-744 (HIHI..LLSV) and
            // calcoutRecord.dbd.pod:1103+ (same). `sub` carries the same
            // HIHI/HIGH/LOLO/LOW + HHSV/HSV/LSV/LLSV set
            // (subRecord.dbd.pod:569-642) and runs the analog `checkAlarms`.
            // `scalcout` declares the identical set (`sCalcoutRecord.dbd:479-531`
            // HIHI/LOLO/HIGH/LOW/HHSV/LLSV/HSV/LSV/HYST + `:858` LALM) and its
            // `checkAlarms` (`sCalcoutRecord.c:699-752`) is the same ladder, run
            // BEFORE the OOPT switch (`:374`) precisely so a limit excursion can
            // drive IVOA. Without the slot the record had no alarm surface at
            // all: `caput scalc.HIHI 5` was a `FieldNotFound` and a scalcout
            // could never go MINOR/MAJOR on its own result.
            //
            // **This match is the single owner of "which records have the analog
            // ladder"** — `evaluate_alarms` runs it off the slot's presence, so a
            // record added here gets the ladder and one absent cannot.
            "ai" | "ao" | "longin" | "longout" | "int64in" | "int64out" | "calc" | "calcout"
            | "sub" | "scalcout" => Some(AnalogAlarmConfig::default()),
            _ => None,
        };
        let mut common = CommonFields::default();
        common.analog_alarm = analog_alarm;

        Self {
            destroyed: false,
            name,
            record,
            common,
            subscribers: HashMap::new(),
            parsed_inp: ParsedLink::None,
            parsed_out: ParsedLink::None,
            parsed_flnk: ParsedLink::None,
            parsed_sdis: ParsedLink::None,
            parsed_tsel: ParsedLink::None,
            device: None,
            subroutine: None,
            pact: AtomicBool::new(false),
            notify: None,
            notify_restart_list: std::collections::VecDeque::new(),
            last_posted: HashMap::new(),
            declared_overrides: HashMap::new(),
            link_backed_metadata_links,
            array_hash_changed: false,
            suppress_subroutine_run: false,
            reprocess_generation: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            watchdog_generation: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            info: HashMap::new(),
            metadata_cache: StdMutex::new(None),
        }
    }

    /// **The owner of a record's init passes** — C `iocInit.c::doInitRecord0`
    /// (`:508-536`) and `doInitRecord1`. Nothing else may call
    /// `Record::init_record`.
    ///
    /// C runs a prologue on EVERY record before pass 0, and it is the reason
    /// this is one function instead of two `init_record` calls at each caller:
    ///
    /// ```c
    /// /* Reset the process active field */
    /// precord->pact = FALSE;
    ///
    /// /* Initial UDF severity */
    /// if (precord->udf && precord->stat == UDF_ALARM)
    ///     precord->sevr = precord->udfs;
    /// ```
    ///
    /// A record is born `udf = 1`, `stat = UDF_ALARM` (dbCommon.dbd
    /// `initial("UDF")`), `udfs = INVALID` — so after `iocInit` a record that
    /// has NEVER processed advertises `STAT=UDF SEVR=INVALID`, not
    /// `NO_ALARM`. That is what makes an `MS` consumer inherit
    /// `LINK`/`INVALID` from a not-yet-processed source, the IOC-startup
    /// ordering case MS exists for (softIoc-verified). A record whose
    /// `init_record` or device support defines the value clears UDF and the
    /// severity goes away on its first process.
    ///
    /// `name` is used only for the init-failure diagnostics C sends to errlog.
    ///
    /// Crate-private on purpose: the passes must run against the record's FINAL
    /// loaded field set (the initial UDF severity is a function of UDF/STAT/
    /// UDFS, and a `.db` `field(VAL,…)` clears UDF at load — C
    /// `dbStaticLib.c:2653-2661`). The one caller is the creation sink,
    /// [`crate::server::database::PvDatabase::add_loaded_record`], which takes
    /// the load and the record together so no path can init a half-loaded
    /// record.
    pub(crate) fn run_init_passes(&mut self, name: &str) {
        // C's `precord->pact = FALSE` — a record cannot be mid-process at init,
        // so this release provably frees nothing: no client put has run, so the
        // restart list is empty.
        debug_assert!(
            self.notify_restart_list.is_empty(),
            "a record cannot hold a queued put-notify at init"
        );
        let _ = self.leave_pact();
        if self.common.udf != 0
            && self.common.stat == crate::server::recgbl::alarm_status::UDF_ALARM
        {
            self.common.sevr = AlarmSeverity::from_u16(self.common.udfs as u16);
        }
        if let Err(e) = self.record.init_record(0) {
            eprintln!("init_record(0) failed for {name}: {e}");
        }
        if let Err(e) = self.record.init_record(1) {
            eprintln!("init_record(1) failed for {name}: {e}");
        }
        // The UDF tail of pass 1. `init_record` cannot reach UDF (a common
        // field), so the record types whose C `init_record` ends in
        // `prec->udf = FALSE` — histogram's `clear_histogram`, aao's constant
        // DOL, mbboDirect's B0..B1F fold (epics-base dabcf89) — deliver it
        // through this hook instead. It lives HERE, inside the init owner,
        // because it is part of the same C pass: a creation path that ran the
        // passes but skipped the tail (iocsh `dbLoadRecords` did) left those
        // records UDF=1 where C has UDF=0.
        // The `post_init_finalize_undef` hook is a cross-crate record-trait API
        // over a `bool` (histogram/aao/mbboDirect implement it); bridge the raw
        // `u8` carrier through it here at the single init owner.
        let mut udf = self.common.udf != 0;
        if let Err(e) = self.record.post_init_finalize_undef(&mut udf) {
            eprintln!("post_init_finalize_undef failed for {name}: {e}");
        }
        self.common.udf = udf as u8;
        // C `init_record` that ends in `prec->udf = 0; recGblResetAlarms(prec)`
        // — the asyn record, defined and no-alarm the moment it loads. The born
        // `UDF`/`INVALID` (and the UDF-severity derivation above) are overwritten
        // here: at init `nsta`/`nsev` are 0, so `rec_gbl_reset_alarms` transfers
        // `STAT`/`SEVR` to `NO_ALARM`. Runs after `post_init_finalize_undef` so
        // it is the final word on this record's initial alarm state.
        if self.record.init_resets_alarms() {
            self.common.udf = 0;
            let _ = crate::server::recgbl::rec_gbl_reset_alarms(&mut self.common);
        }
        // C `init_record` can END with `prec->pact = TRUE` to disable a record
        // it cannot process (`subRecord.c:119-123`, an empty SNAM). PACT has one
        // owner, so the record answers the predicate and the owner parks it —
        // after the passes, so the `leave_pact()` above cannot undo it. The
        // release is not lost: a put to a `pact_park_fields()` field re-asks,
        // the way C's `special()` does.
        if self.record.parks_pact() {
            self.enter_pact();
        }
    }

    /// SINGLE OWNER of the DTYP -> soft-output-dset mapping. The dset table
    /// decides what a soft OUT-link write carries; no caller may re-derive it.
    ///
    /// C ships two soft output dsets per output record type and DTYP picks one:
    /// `devXxxSoft.c::write_xxx` puts VAL/OVAL on the OUT link, while
    /// `devXxxSoftRaw.c::write_xxx` puts the RAW word — `dbPutLink(&prec->out,
    /// DBR_LONG, &prec->rval, 1)` (`devAoSoftRaw.c:44`, `devBoSoftRaw.c:65`) or
    /// `data = prec->rval & prec->mask` (`devMbboSoftRaw.c:71-75`,
    /// `devMbboDirectSoftRaw.c:71-75`).
    ///
    /// `Record::raw_soft_output_value` IS the SoftRaw column of that table:
    /// `Some` exactly for the record types C ships a SoftRaw dset for. A record
    /// type C has no SoftRaw dset for keeps the plain soft-channel value —
    /// `DTYP="Raw Soft Channel"` on a `longout` is a `.db` error C rejects at
    /// init ("no device support"), and the port's lenient reading of it (the
    /// same one [`crate::server::device_support::is_soft_dtyp`] already applies
    /// on the input side) must not turn the write into a silent no-op.
    ///
    /// `None` means DTYP names device support that owns the write — real
    /// hardware. "Async Soft Channel" is NOT that: C's
    /// `devXxxSoftCallback.c::write_xxx` puts the same VAL/OVAL the plain soft
    /// dset puts, only through `dbPutLinkAsync` (`devAoSoftCallback.c:49`,
    /// `devLoSoftCallback.c:49`), and falls back to a synchronous `dbPutLink`
    /// when the link has no LSET. Returning `None` for it made every
    /// `DTYP("Async Soft Channel")` output record write nothing at all —
    /// measured on `pva2pva/testApp/testpvalink.db:30-35`, whose `longout`
    /// drives a pva OUT link that never fired.
    pub fn soft_output_value(&self) -> Option<Option<EpicsValue>> {
        use crate::server::device_support::{SoftDtyp, classify_soft};
        match classify_soft(&self.common.dtyp)? {
            SoftDtyp::Raw => Some(
                self.record
                    .raw_soft_output_value()
                    .or_else(|| self.record.output_link_value()),
            ),
            SoftDtyp::Plain | SoftDtyp::Async => Some(self.record.output_link_value()),
        }
    }

    /// Set a single `info("key", "value")` tag on this record. Last
    /// write wins. Used by the .db loader (`info(...)` directive) and
    /// `dbpf`-style tools.
    pub fn set_info(&mut self, key: impl Into<String>, value: impl Into<String>) {
        self.info.insert(key.into(), value.into());
    }

    /// Look up a single info tag. Returns `None` when the record has
    /// no tag with that key.
    pub fn get_info(&self, key: &str) -> Option<&str> {
        self.info.get(key).map(|s| s.as_str())
    }

    /// The value of `field` already published to its `DBE_VALUE`/`DBE_LOG`
    /// subscribers, or `None` when the framework has never published one.
    /// The read side of the `last_posted` contract — see the field's docs.
    pub(crate) fn posted_value(&self, field: &str) -> Option<&EpicsValue> {
        self.last_posted.get(field)
    }

    /// SINGLE OWNER of `last_posted`: record that `value` has been published
    /// to `field`'s `DBE_VALUE`/`DBE_LOG` subscribers, so no later cycle
    /// change-detects and re-publishes it.
    ///
    /// Every value-class post — the snapshot builders' change-detected posts,
    /// the intermediate async-notify posts, and the put-time
    /// [`Self::notify_field_with_origin`] post that C makes from `dbPut`
    /// (dbAccess.c:1414) — routes through here. Alarm-only / property-only
    /// posts MUST NOT call it: they deliver nothing to a value-class
    /// subscriber, so the value is still owed.
    pub(crate) fn record_value_post(&mut self, field: &str, value: EpicsValue) {
        if let Some(slot) = self.last_posted.get_mut(field) {
            *slot = value;
        } else {
            self.last_posted.insert(field.to_string(), value);
        }
    }

    /// Invalidate the metadata cache. Called after writing any
    /// metadata-class field (EGU, PREC, HOPR/LOPR, alarm limits,
    /// DRVH/DRVL, enum strings). The next snapshot will rebuild the
    /// cache from the new values.
    pub fn invalidate_metadata_cache(&self) {
        if let Ok(mut guard) = self.metadata_cache.lock() {
            *guard = None;
        }
    }

    /// **The** `DBE_PROPERTY` gate: C `dbAccess.c:1330`
    /// `paddr->pfldDes->prop`, read from the field's own declaration.
    ///
    /// C never consults a list of field names — it asks the `.dbd`, per record
    /// type, which is why `histogram.ULIM` is a property and `bi.ZSV` (declared
    /// `pp(TRUE)`, no `prop`) is not, and why `bi.ZNAM` is one while
    /// `busy.ZNAM` is not. Asking [`Self::field_desc`] gives the port the same
    /// per-type answer from the same generated `.dbd` tables.
    ///
    /// A field with no declaration at all — a virtual field (`RTYP`, `TIME`) —
    /// has no `dbFldDes` in C either, so it is not property-class.
    pub(crate) fn field_posts_property(&self, field: &str) -> bool {
        self.field_desc(field).is_some_and(|d| d.prop)
    }

    /// Hook called by the database after a field is written. If the field is a
    /// metadata-cache source, the cache is invalidated so the next snapshot
    /// picks up the new value. Posts nothing — a caller that also owes the
    /// `DBE_PROPERTY` event uses [`Self::notify_field_written_if_changed`].
    ///
    /// Field name is automatically uppercased.
    pub fn notify_field_written(&self, field: &str) {
        let upper = field.to_ascii_uppercase();
        if is_metadata_cache_source(&upper) {
            self.invalidate_metadata_cache();
        }
    }

    /// Like [`Self::notify_field_written`], plus the `DBE_PROPERTY` post C
    /// makes from `dbPut` — and both are skipped when the put did not actually
    /// change the field's value. Mirrors epics-base `faac1df1`: property events
    /// fire only on real changes, not on idempotent writes (the C path compares
    /// `paddr->pfield` against the converted payload before setting the
    /// `propertyUpdate` flag).
    ///
    /// The two effects have independent gates. Invalidation follows
    /// `is_metadata_cache_source` (what this port's cache reads); the post
    /// follows `Self::field_posts_property` (what the `.dbd` declares). A
    /// field can be either without being both.
    ///
    /// `prev` is the value captured BEFORE the put. Callers that don't need the
    /// change-detection (e.g. internal writers that know the field is neither)
    /// can keep using [`Self::notify_field_written`].
    ///
    /// `backing` is what the sweep needs and could not have: the post below
    /// names EVERY subscribed field, so it reaches a link-backed one whenever a
    /// client is monitoring it, and this method runs under the record's own
    /// write lock where the target's lock cannot be taken. The put path that
    /// calls it has already resolved one at its no-lock point.
    pub fn notify_field_written_if_changed(
        &mut self,
        field: &str,
        prev: Option<&EpicsValue>,
        backing: LinkBacking<'_>,
    ) {
        let upper = field.to_ascii_uppercase();
        let cache_source = is_metadata_cache_source(&upper);
        let posts_property = self.field_posts_property(&upper);
        if !cache_source && !posts_property {
            return;
        }
        let now = self.record.get_field(&upper);
        if prev == now.as_ref() {
            return;
        }
        if cache_source {
            self.invalidate_metadata_cache();
        }
        if posts_property {
            // mirror C dbAccess.c:1395-1396 — the gate `if (propertyUpdate &&
            // !status)` and the `db_post_events(precord, NULL, DBE_PROPERTY)` it
            // guards. The NULL field pointer is what makes it record-wide.
            // Collect keys first to avoid a re-entrant immutable borrow on subscribers.
            let fields: Vec<String> = self.subscribers.keys().cloned().collect();
            for f in fields {
                self.notify_field_with_origin(
                    &f,
                    crate::server::recgbl::EventMask::PROPERTY,
                    0,
                    backing,
                );
            }
        }
    }

    /// Returns the cached MetadataSnapshot, building and storing it on
    /// the first call (or after invalidation). Used by both
    /// `snapshot_for_field` and `make_monitor_snapshot` so the populate
    /// cost is paid at most once per metadata-stable interval.
    fn cached_metadata(&self) -> MetadataSnapshot {
        // Fast path: cache hit
        if let Ok(guard) = self.metadata_cache.lock()
            && let Some(cached) = guard.as_ref()
        {
            return cached.clone();
        }

        // Cache miss: build a fresh metadata snapshot
        let mut tmp = super::super::snapshot::Snapshot::new(
            EpicsValue::Double(0.0),
            0,
            0,
            std::time::SystemTime::UNIX_EPOCH,
        );
        self.populate_display_info(&mut tmp);
        self.populate_control_info(&mut tmp);
        self.populate_enum_info(&mut tmp);

        let meta = MetadataSnapshot {
            display: tmp.display,
            control: tmp.control,
            enums: tmp.enums,
        };

        // Store back; ignore poisoning (cache is best-effort).
        if let Ok(mut guard) = self.metadata_cache.lock() {
            *guard = Some(meta.clone());
        }
        meta
    }

    /// C `dbChannelSpecial(chan) == SPC_NOMOD` — **the single owner of the
    /// no-modify declaration**, for every consumer that needs to know whether a
    /// field can be written.
    ///
    /// C declares it once, in the `.dbd`, and reads it in two unrelated places:
    ///
    /// * `dbPut` (`dbAccess.c:123-126`, via `dbPutSpecial(paddr, 0)`) refuses
    ///   the write — the port's `check_no_mod` gate;
    /// * `rsrvCheckPut` (`rsrv/camessage.c:2540-2551`) — `if
    ///   (dbChannelSpecial(pciu->dbch) == SPC_NOMOD) return 0;` — which feeds
    ///   the CA `ACCESS_RIGHTS` write bit (`camessage.c:1154-1156`) as well as
    ///   both put paths, so a client sees `Access: read, no write` and never
    ///   sends the doomed write.
    ///
    /// Only the first consumer existed in the port, so every dbCommon NOMOD
    /// field advertised WRITE on the wire (`caput N1.SEVR 2` was refused
    /// server-side, after the client had already sent it, with an async
    /// exception instead of C's clean client-side "Write access denied").
    ///
    /// Three sources, one answer:
    ///
    /// 1. the dbCommon `SPC_NOMOD` set below — common fields, so no record's
    ///    `field_list` declares them;
    /// 2. the record type's **declaration**, resolved by `Self::field_desc` —
    ///    the vendored `.dbd` whenever one exists, and only for a record type
    ///    that has no `.dbd` at all (`motor`, `optics`, `scaler`, `std`) the
    ///    record's own hand-written table, which for those Tier 3 types
    ///    genuinely *is* their declaration;
    /// 3. [`Record::field_no_mod`] — an SPC_NOMOD a record's `cvt_dbaddr`
    ///    raises from its own state (compress VAL under BALG=LIFO,
    ///    `compressRecord.c:404-405`), which a static `FieldDesc` cannot
    ///    express.
    ///
    /// `field` may be any case.
    pub fn is_no_mod(&self, field: &str) -> bool {
        if DBCOMMON_NOMOD.iter().any(|f| f.eq_ignore_ascii_case(field)) {
            return true;
        }
        if self.field_desc(field).is_some_and(|f| f.read_only) {
            return true;
        }
        self.record.field_no_mod(field)
    }

    /// Check if the record is currently processing (PACT equivalent).
    pub fn is_processing(&self) -> bool {
        self.pact.load(std::sync::atomic::Ordering::Acquire)
    }

    /// C `prec->pact = TRUE` — the record goes busy for an async device
    /// round-trip, an SDLY simulation defer, or an ODLY reprocess window.
    pub fn enter_pact(&self) {
        self.pact.store(true, std::sync::atomic::Ordering::Release);
    }

    /// C `prec->pact = FALSE` — the ONLY release of PACT.
    ///
    /// The returned [`PactExit`] carries the release's debt to the cycle tail,
    /// where a queued put-notify is restarted — the omission the open-coded
    /// `processing.store(false)` at the ODLY continuation and the three SIM/SDLY
    /// releases made.
    ///
    /// `#[must_use]` does NOT enforce that debt and never did: the lint fires on
    /// an unused *expression*, so a site that binds the token with `let` and then
    /// leaves by `?` or an early `return` warns about nothing. The enforcement is
    /// `processing::CycleEndGuard`, whose `Drop` pays the tail for every exit
    /// that did not.
    pub fn leave_pact(&mut self) -> PactExit {
        self.pact.store(false, std::sync::atomic::Ordering::Release);
        PactExit::new(self.notify_restart_pending())
    }

    /// The cycle-tail token for a record this cycle did NOT release PACT on.
    ///
    /// Still consults the queue: a notify parked behind an in-flight wait-set
    /// on an idle record is freed by the wait-set completion, and the tail is
    /// what promotes it.
    pub fn pact_exit_without_release(&self) -> PactExit {
        PactExit::new(self.notify_restart_pending())
    }

    /// C `processNotifyCommon`'s two defer tests (dbNotify.c:213, 225), as one
    /// question: may a NEWLY ARRIVING put-notify take this record now?
    ///
    /// `true` for an in-flight wait-set (`precord->ppn`), for PACT, and for a
    /// non-empty restart list — the last so a notify arriving in the window
    /// between a completion and the restart check cannot jump the queue.
    ///
    /// A RESTARTED put is not asked this: it is already the record's owner (C
    /// `precord->ppn == ppn`, state `notifyRestartCallbackRequested`, which
    /// dbNotify.c:213 exempts by name) and only PACT can stop it — see
    /// `Self::requeue_notify_put`.
    pub fn notify_put_is_owned(&self) -> bool {
        self.notify.is_some() || self.is_processing() || !self.notify_restart_list.is_empty()
    }

    /// C `processNotifyCommon`'s FIRST defer test alone (dbNotify.c:213):
    /// another `processNotify` owns this record, or one is already queued
    /// behind it. [`Self::notify_put_is_owned`] folds in the PACT arm
    /// (`:225`) as well.
    ///
    /// A DBF link-field put waits on ownership but NOT on PACT. A bare `sub`
    /// with an empty `SNAM` parks PACT=TRUE forever (subRecord.c:119-122), so
    /// a link put that waited on the PACT arm there would never be written and
    /// `caput <sub>.INPA 0` would read back empty. Ownership carries no such
    /// trap: the restart check drains the queue at every cycle end.
    pub fn notify_put_has_owner(&self) -> bool {
        self.notify.is_some() || !self.notify_restart_list.is_empty()
    }

    /// C `ellSafeAdd(&precord->ppnr->restartList, &ppn->restartNode)` — the
    /// arriving put-notify joins the back of the queue, unwritten.
    ///
    /// Infallible: C has no "refuse" arm here, and a refusal loses the client's
    /// write. Call only under [`Self::notify_put_is_owned`].
    /// Take this record's put-notify slot, or queue behind whoever holds it.
    ///
    /// C `processNotifyCommon` (dbNotify.c:211-231) has exactly two outcomes
    /// and no third: the record is free and the notify takes it, or it is
    /// owned and the notify joins `precord->ppnr->restartList`. There is no
    /// refusal arm — `ECA_PUTCBINPROG` has one sender in all of base, the
    /// 60-second put-callback timeout in `write_notify_action`
    /// (`rsrv/camessage.c:1701` at R7.0.10).
    ///
    /// `None` means queued, and the caller MUST NOT process: the replay
    /// drives the record and fires the callback, so processing here would
    /// run the cycle twice for one client request.
    ///
    /// Ownership alone decides — NOT [`Self::notify_put_has_owner`]. A
    /// non-empty restart list stops a *fresh* arrival at the entry gate, but a
    /// replay reaching here has already been popped off that list and must
    /// take the slot with its successors still queued behind it, exactly as
    /// C `restartCheck` (dbNotify.c:158-168) assigns `precord->ppn = pfirst`
    /// while leaving the rest of `restartList` in place.
    pub fn install_or_queue_notify(
        &mut self,
        completion: crate::runtime::sync::oneshot::Sender<()>,
    ) -> Option<Arc<NotifyWaitSet>> {
        if self.notify.is_some() {
            self.queue_notify_put(DeferredNotify::Process { completion });
            return None;
        }
        let notify = NotifyWaitSet::for_entry_record(&self.name, completion);
        self.notify = Some(notify.clone());
        Some(notify)
    }

    /// C `dbNotifyAdd` (dbNotify.c:477-501): a link target joins the wait-set
    /// of the put-notify driving the chain, so the initiator's completion
    /// waits for this record's cycle too.
    ///
    /// The second and last writer of `Self::notify`; the first is
    /// [`Self::install_or_queue_notify`]. Both live here so the slot has no
    /// assignment site outside this module — an open-coded one elsewhere is
    /// how a wait-set came to be installed without the record's write gate.
    ///
    /// A record already carrying a wait-set keeps it (C's `if (!pto->ppn …)`
    /// at `:492`), so this never displaces a live one, and the `enter` is
    /// paired with the `leave` the target's own cycle tail performs.
    pub fn join_put_notify(&mut self, src: Option<&Arc<NotifyWaitSet>>) {
        if self.notify.is_some() {
            return;
        }
        if let Some(ws) = src {
            self.notify = Some(ws.clone());
            ws.enter();
        }
    }

    /// Give up a claim on the slot without ever having processed under it.
    ///
    /// NOT a completion. `complete_put_notify` (`processing.rs:449`, C
    /// `dbNotifyCompletion`) `leave`s the wait-set because the record
    /// contributed a cycle to it; an abandoned claim contributed nothing, so
    /// the set is dropped whole. Its `pending` never reaches zero, and the
    /// client's receiver wakes on the dropped sender — the same release C
    /// gives a `dbNotifyCancel`.
    ///
    /// The caller must be the claim's owner. Nothing else can have cleared or
    /// replaced the slot in between: [`Self::install_or_queue_notify`] and
    /// [`Self::join_put_notify`] both refuse an occupied slot, and
    /// [`Self::take_next_notify_restart`] will not pop while it is occupied,
    /// so the assertion below states an invariant rather than guarding a
    /// race.
    pub(crate) fn abandon_put_notify(&mut self, claimed: &Arc<NotifyWaitSet>) {
        let taken = self.notify.take();
        debug_assert!(
            taken.as_ref().is_some_and(|ws| Arc::ptr_eq(ws, claimed)),
            "only the claim owner may clear the put-notify slot"
        );
    }

    /// Whether a put-notify owns this record — C `precord->ppn != NULL`.
    ///
    /// The public read of the slot. The wait-set itself stays crate-private so
    /// no caller outside this crate can `enter`/`leave` a set it does not own,
    /// which is the accounting [`NotifyWaitSet`] exists to keep.
    pub fn has_notify(&self) -> bool {
        self.notify.is_some()
    }

    pub fn queue_notify_put(&mut self, put: DeferredNotify) {
        debug_assert!(
            self.notify_put_is_owned(),
            "a put-notify is queued only when the record is owned; otherwise it \
             takes the record directly"
        );
        self.notify_restart_list.push_back(put);
    }

    /// C `processNotifyCommon`'s `precord->pact` arm reached by a RESTARTED
    /// notify (dbNotify.c:225-231): it stays `precord->ppn` and waits for the
    /// next completion, so it does NOT fall in behind puts that arrived after
    /// it. Back to the head.
    ///
    /// The only way a promotion can find the record busy is a scan that took
    /// PACT between the pop and the replay; the record's advisory write gate,
    /// held across both, keeps every other put out of that window.
    pub(crate) fn requeue_notify_put(&mut self, put: DeferredNotify) {
        debug_assert!(
            self.is_processing(),
            "a promoted put-notify returns to the head only because the record \
             went PACT under it"
        );
        self.notify_restart_list.push_front(put);
    }

    /// C `restartCheck` (dbNotify.c:149-170) — promote the queue head once the
    /// record is free, or leave it queued for the next completion.
    ///
    /// **The only drain.** The freedom test lives here rather than at the call
    /// site, so promoting onto a record that is still PACT or still carries a
    /// wait-set is not expressible.
    pub(crate) fn take_next_notify_restart(&mut self) -> Option<DeferredNotify> {
        if self.notify.is_some() || self.is_processing() {
            return None;
        }
        self.notify_restart_list.pop_front()
    }

    /// Does this record owe anyone a restart? The cheap read that keeps the
    /// per-cycle restart check off the spawn path when nothing is queued.
    pub(crate) fn notify_restart_pending(&self) -> bool {
        !self.notify_restart_list.is_empty()
    }

    /// How many put-notifies are queued behind whoever owns this record — C
    /// `ellCount(&precord->ppnr->restartList)`.
    ///
    /// A count and not a bool because `dbNotifyDump` prints one line per
    /// queued entry (`dbNotify.c:678-685`); [`Self::notify_restart_pending`]
    /// answers the cheaper question the restart check asks. Read-only: the
    /// queue's only drain is still [`Self::take_next_notify_restart`].
    pub(crate) fn notify_restart_len(&self) -> usize {
        self.notify_restart_list.len()
    }

    /// Unified field resolution: record fields → common fields → virtual
    /// fields — and, for a link field, C `dbGet`'s rendering of it.
    ///
    /// This is the port's `dbGet` (`dbAccess.c:625-961`): the read every
    /// external reader arrives at, whether it came from
    /// [`PvDatabase::get_pv`](crate::server::database::PvDatabase::get_pv)
    /// on behalf of a CA client, from `dbgf`, or from `dbpr`. C's `dbGet`
    /// sends `DBF_INLINK`/`DBF_OUTLINK`/`DBF_FWDLINK` to `getLinkValue`
    /// (`:944-947`), which renders the link with `dbGetString` (`:850-856`),
    /// so applying that here is what makes every reader agree without any of
    /// them knowing the rule.
    ///
    /// The STORE is still the text — `Record::get_field` — and that is what
    /// the link layer parses. The two are not the same value and do not share
    /// a name: C likewise reads `precord->inp` directly when it wants the
    /// link and `dbGet` when it wants what a client would see.
    pub fn resolve_field(&self, name: &str) -> Option<EpicsValue> {
        let name = name.to_ascii_uppercase();
        let value = self.resolve_field_stored(&name)?;
        Some(self.as_a_reader_sees(&name, value))
    }

    /// [`Self::resolve_field`] without the reader's view — what the field
    /// HOLDS, which for a link field is the text C's `dbParseLink` takes
    /// (`dbStaticLib.c:2246`) rather than what `dbGetString` renders
    /// (`:1906-2050`).
    ///
    /// `dbpr` needs both of the same field, and in C they come from one
    /// address: it prints the link's resolved TYPE in front of the rendered
    /// text (`dbTest.c:1205-1224`). Splitting the accessor chain here keeps
    /// that one address — a second walk to find the stored text would be a
    /// second answer to "which field is this", and the round before this one
    /// is what happens when those two disagree.
    ///
    /// `name` must already be upper-case.
    pub fn resolve_field_stored(&self, name: &str) -> Option<EpicsValue> {
        let value = match self.field_desc(name) {
            // C `dbGet`'s validity gate (`dbAccess.c:667-675`): the NAME
            // resolves — `dbNameToAddr` finds every field the `.dbd` declares
            // — and the READ is what fails, with `S_db_badDbrtype`. The two
            // outcomes leave by different doors one level up, in
            // `PvDatabase::get_pv`, which turns a declared-but-unresolved
            // field into `CaError::BadDbrType` and an undeclared one into
            // `ChannelNotFound`.
            //
            // The gate goes HERE, in front of the accessor chain, rather than
            // in whichever accessor would otherwise answer: `declared_default`
            // synthesises the declared type's zero for any field with no
            // stored value, so leaving the row to reach it served `REC.TIME`
            // as `UChar(0)` — a value C has no way to produce.
            Some(desc) if desc.unreadable() => return None,
            // C `dbFindFieldPart` — the record type's own `.dbd` table, then
            // `dbCommon`. Every accessor below reads STORAGE, and this port
            // keeps a good deal of storage on `CommonFields` that C keeps per
            // record type (`INP`, `OUT`, `SSCN`, the analog-alarm ladder), so
            // without the declaration in front of them a `calc` answered
            // `.OUT` with an empty string where C answers `PV 'C:GOOD.OUT'
            // not found`. The declaration is the namespace, not the storage.
            Some(_) => self
                .record
                .get_field(name)
                .or_else(|| self.get_common_field(name))
                .or_else(|| self.get_virtual_field(name))
                .or_else(|| self.declared_overrides.get(name).cloned())
                .or_else(|| self.declared_default(name))?,
            // C `dbNameToAddr` falls through to `dbGetAttributePart` on
            // `S_dbLib_fieldNotFound` (`dbAccess.c:672-675`), which is how
            // `RTYP` — declared by no record type — reads as the type name.
            // `VERS` and any `dbPutAttribute` name need the database's
            // attribute map and are answered a level up, in `get_pv`.
            None => self.get_virtual_field(name)?,
        };
        Some(value)
    }

    /// C `dbGet`'s link arm applied to one resolved field: a link field reads
    /// as [`render_link_field`], everything else as itself.
    ///
    /// The class lookup is behind the string test because only a string-valued
    /// field can be a link, and the numeric fields a processing cycle reads
    /// (`HASH`, `SIMM`, `SDLY`) must not pay for a declaration scan.
    fn as_a_reader_sees(&self, upper_field: &str, value: EpicsValue) -> EpicsValue {
        let EpicsValue::String(ref text) = value else {
            return value;
        };
        let Some(class) = crate::types::dbf_link_class(self.record.record_type(), upper_field)
        else {
            return value;
        };
        EpicsValue::String(
            render_link_field(class, text.as_str_lossy().as_ref())
                .as_str()
                .into(),
        )
    }

    /// The value a field that is DECLARED by the `.dbd` but has no live store
    /// on this record serves: its `initial(...)`, or a type-zero.
    ///
    /// C makes *every* `.dbd` field addressable — `dbNameToAddr` resolves the
    /// field from its `dbFldDes` and `dbGet` reads it out of record memory,
    /// which the dbd loader seeded with `initial()` (or left zero). A Rust
    /// record implements only the fields it has behaviour for, so a field it
    /// declares but never touches — `aSub.OVAL`, `sub.LA`, `sel.HOPR` — had no
    /// channel at all: [`Self::resolve_field`]'s three accessors all returned
    /// `None` and CA create-channel answered `S_dbLib_recNotFound`.
    ///
    /// The declared table is the contract for *which* fields exist; this is the
    /// last resort for the *value* of one with no runtime accessor, and it is
    /// exactly what an unprocessed C record on an empty `.db` returns —
    /// [`apply_dbd_initials`](crate::server::db_loader) seeds the same
    /// `initial()` into the fields the record *does* store, from the same
    /// generated table, so the two paths agree by construction.
    fn declared_default(&self, name: &str) -> Option<EpicsValue> {
        let desc = self.field_desc(name)?;
        // A `runtime_typed` field (`VAL`/`BG`, re-typed from `FTVL`/`SDEF`) is
        // record-owned by definition and its placeholder `dbf_type` is not what
        // it serves; never synthesise one here — the record itself answers it.
        if desc.runtime_typed {
            return None;
        }
        let initial = desc.initial.unwrap_or("");
        if let Some(choices) = desc
            .menu
            .or_else(|| self.record.menu_field_choices(name))
            .or_else(|| super::shared_menu_choices(name))
        {
            // A menu field with no `initial(...)` is index 0, exactly as an
            // empty numeric field is 0 below.
            if initial.is_empty() {
                return Some(EpicsValue::Enum(0));
            }
            return super::resolve_menu_field_string_db_load(name, choices, desc.dbf_type, initial)
                .ok();
        }
        // `parse` maps an empty string to the declared type's zero, so this one
        // call serves both `initial(...)` and no-initial fields.
        EpicsValue::parse_bytes(desc.dbf_type, initial.as_bytes()).ok()
    }

    /// Resolve a field for EPICS `$` long-string (character-array) access.
    ///
    /// The `$` channel-name modifier (C `dbChannel.c:486-505`) re-views a
    /// field as a `DBR_CHAR` array: a `DBF_STRING` field becomes a char
    /// array of `field_size` elements, a link field a char array of
    /// `PVLINK_STRINGSZ`, and every other field type is rejected with
    /// `S_dbLib_fieldNotFound`. pvxs serves that char view as a
    /// `form = "String"` long-string `NTScalar` — it reads the `DBR_CHAR`
    /// bytes and NUL-terminates them back into a string
    /// (`ioc/iocsource.cpp:133-136`, `ioc/channel.cpp:62-74`).
    ///
    /// Both `DBF_STRING` fields and link fields resolve to an
    /// [`EpicsValue::String`] in this database (a link resolves to its
    /// textual form, see [`Self::get_common_field`]), so a field is
    /// `$`-eligible exactly when it resolves to a string value. Returns
    /// that string value for an eligible field, or `None` for a field the
    /// `$` modifier cannot view as a char array (the
    /// `S_dbLib_fieldNotFound` case) — the single owner of the
    /// dbChannel `$`-eligibility rule for the channel-resolution layer.
    pub fn resolve_string_view_field(&self, name: &str) -> Option<EpicsValue> {
        match self.resolve_field(name)? {
            v @ EpicsValue::String(_) => Some(v),
            _ => None,
        }
    }

    /// Choice table for a field served as `DBR_ENUM` from a `DBF_MENU`:
    /// the record's own record-specific menu
    /// ([`Record::menu_field_choices`]),
    /// else a shared menu keyed by field name
    /// ([`shared_menu_choices`](super::menu_choices::shared_menu_choices)).
    /// The choices a `menu()` field serves as its `DBR_ENUM` labels.
    ///
    /// The `.dbd` declaration is the first and best answer: a generated
    /// [`FieldDesc`] carries the field's own `menu(...)` choices, which is what
    /// C's `dbGetFieldIndex` -> `pamapdbfType` -> menu lookup resolves. The two
    /// hand-maintained fallbacks below are for record types still on a
    /// hand-written table; they go away with the last of them.
    ///
    /// `shared_menu_choices` in particular keys on the field NAME alone, across
    /// every record type — which is only correct while no two record types give
    /// the same field name different menus. Asking the field's own descriptor
    /// first removes that assumption.
    fn menu_choices_for(&self, field: &str) -> Option<&'static [&'static str]> {
        menu_choices_of(self.record.as_ref(), field)
    }

    /// The choices this record's `DTYP` selects among — C's `dbDeviceMenu` for
    /// the record type, in `.dbd` declaration order.
    ///
    /// C's DTYP field IS the index into this list, and an unset DTYP is index 0
    /// — which is why a bare `record(ai,"X"){}` serves `Soft Channel` and a
    /// `record(calc,"X"){}`, whose record type declares no device support at
    /// all, serves the empty string.
    ///
    /// The port stores the device NAME rather than the index, because the name
    /// is what the device-support registry dispatches on, and a name registered
    /// at runtime by a downstream crate (`asynInt32`) has no `device()` line in
    /// any vendored `.dbd`. Such a name is appended as its own slot, so the
    /// index and the string still name the SAME device support: there is no
    /// value of DTYP that renders as a device this record is not bound to.
    /// `None` when the record type declares NO device support at all — C's
    /// `dbDeviceMenu *pdevs = paddr->pfldDes->ftPvt; if (!pdevs) goto nostrs;`
    /// (`dbAccess.c:176-179`), which clears `DBR_ENUM_STRS` so the client is
    /// sent no choice list at all.
    ///
    /// C keeps that case DISTINCT from a device menu that exists but is empty,
    /// and says so at `dbAccess.c:205`: *"indicate option data not available.
    /// distinct from no_str==0"*. An empty-but-present menu is still marked,
    /// with `no_str = 0`; a missing menu is not marked. Returning `Vec` here
    /// and defaulting the missing menu to `[]` collapsed the two, so a
    /// `record(calc,"X"){}` — whose record type has no `device()` line — served
    /// `value.choices = {0}[]` where QSRV2 omits the leaf entirely.
    pub(crate) fn device_choices(&self) -> Option<Vec<PvString>> {
        let record_type = self.record.record_type();
        // Base's build-time menu (`epics-base-rs/dbd`), then the menus a
        // downstream crate whose device support has no vendored `device()` line
        // registered at runtime (asyn's `asynInt32`, `asynFloat64`, ...). C's
        // `dbDeviceMenu` is the concatenation of every `device()` the loaded
        // `.dbd` set declares, in load order — base first, then asyn — so the
        // merge appends the contributed choices AFTER the declared ones.
        // The C None-vs-empty distinction (`dbAccess.c:176-179` vs `:205`): the
        // menu is present iff the loaded `.dbd` set declares ANY `device()` for
        // this type. A type base declares none for but asyn does (structurally
        // possible, though none of asyn's are such) is therefore present, not
        // None; a type neither declares for (calc) stays None.
        if super::dbd_generated::device_menu(record_type).is_none()
            && super::contributed_device_menu(record_type).is_empty()
        {
            return None;
        }
        // `merged_device_menu` = declared + contributed, the SAME source the
        // CA-put validation (`coerce_put_value`'s DTYP branch) resolves against,
        // so a client can put exactly the DTYP names it can read here.
        let mut names: Vec<PvString> = super::merged_device_menu(record_type)
            .into_iter()
            .map(PvString::from)
            .collect();
        let dtyp = self.common.dtyp.as_str();
        if !dtyp.is_empty() && !names.iter().any(|n| n.as_str_lossy() == dtyp) {
            names.push(PvString::from(dtyp));
        }
        Some(names)
    }

    /// C `dbPutFieldLink`'s link-type gate (`dbAccess.c:1125-1137`): a link
    /// written at RUNTIME is held to the same `dbCanSetLink` rule as one written
    /// by the `.db`, against the device support the record's CURRENT `DTYP`
    /// binds. Same rule, same owner — [`super::check_link_assignment`]; only the
    /// DTYP it is asked about differs (the record's, not the `.db` text's).
    ///
    /// [`MenuBound::DbLoad`] is exempt, and that is not a hole: on the db-load
    /// path C does not check a link as each field is parsed either. It checks
    /// once, at `iocInit`, over the record as loaded (`dbStaticLib.c:2178-2231`)
    /// — which is why `field(INP,…)` may precede `field(DTYP,…)` in a `.db` and
    /// still bind. [`PvDatabase::db_init_record_links`] is that pass, and it
    /// reads the record's DTYP off the record itself, so it does not depend on
    /// the order the `.db` happened to spell its fields in. Gating here as well
    /// would re-introduce exactly that order dependence.
    ///
    /// [`PvDatabase::db_init_record_links`]: crate::server::database::PvDatabase
    fn check_link_assignment(
        &self,
        upper_field: &str,
        text: &str,
        bound: MenuBound,
    ) -> CaResult<()> {
        if matches!(bound, MenuBound::DbLoad) {
            return Ok(());
        }
        super::check_link_assignment(
            self.record.record_type(),
            Some(self.common.dtyp.as_str()),
            upper_field,
            text,
        )
    }

    /// The value of the `DTYP` field: the index of the bound device support in
    /// [`Self::device_choices`]. An unset DTYP is index 0, exactly as in C.
    pub(crate) fn dtyp_index(&self) -> u16 {
        let dtyp = self.common.dtyp.as_str();
        if dtyp.is_empty() {
            return 0;
        }
        // A record type with no device menu has no slot for any DTYP, so the
        // index stays 0 — the same answer the old `unwrap_or(&[])` gave.
        self.device_choices()
            .unwrap_or_default()
            .iter()
            .position(|c| c.as_str_lossy() == dtyp)
            .unwrap_or(0) as u16
    }

    /// **The** owner of "what string does this enum-valued field render as" —
    /// C's `[DBF_*][DBR_STRING]` conversion row, chosen by the field's DBF
    /// class. Every path that renders an enum as a string goes through here:
    /// the CA/PVA encoders (via [`EnumInfo::string_form`](crate::server::snapshot::EnumInfo::string_form) on the
    /// snapshot this builds) and the db-link read
    /// ([`Self::field_as_dbr_string`]). There is exactly one such table per
    /// field, and no path may reconstruct a second one.
    ///
    /// C's dispatch, and this function's, in the same order:
    ///
    /// * `DBF_MENU` / `DBF_DEVICE` -> `getMenuString` / `getDeviceString`, the
    ///   field's own choice list. Asked FIRST, because a menu field on a record
    ///   whose `VAL` is an enum (`bo.OMSL`) must render its menu's choices, not
    ///   the record's `ZNAM`/`ONAM`.
    /// * `DBF_ENUM` `VAL` -> `getEnumString` -> the record's `get_enum_str`
    ///   rset ([`Record::enum_string_form`]).
    ///
    /// `None` when the field has neither — C answers `S_db_noRSET`, an error;
    /// the port renders empty.
    ///
    /// Each class brings its own out-of-range rule with it (see
    /// [`EnumOverflow`](crate::server::snapshot::EnumOverflow)); the index is
    /// rendered as a number for a `DBF_MENU` and ONLY for a `DBF_MENU`.
    pub(crate) fn enum_string_form_for(&self, field: &str) -> Option<EnumStringForm> {
        if field.eq_ignore_ascii_case("DTYP") {
            // `None` propagates C's `goto nostrs` (`dbAccess.c:178`): a record
            // type with no `device()` declaration supplies no choice list, so
            // the leaf is omitted rather than marked empty.
            return self.device_choices().map(EnumStringForm::device);
        }
        if let Some(choices) = self.menu_choices_for(field) {
            return Some(EnumStringForm::menu(
                choices.iter().map(|c| PvString::from(*c)),
            ));
        }
        if field.eq_ignore_ascii_case("VAL") {
            return self.record.enum_string_form();
        }
        None
    }

    /// Is `field` one of the DBF classes C's soft device support writes as
    /// `DBR_STRING`?
    ///
    /// `devsCalcoutSoft.c:128-130` (and its async twin, :83-85) switches the
    /// scalcout OUT put on the TARGET field's DBF type and sends `OSV` — the
    /// string result — for seven of them:
    ///
    /// ```c
    /// case DBF_STRING: case DBF_ENUM: case DBF_MENU: case DBF_DEVICE:
    /// case DBF_INLINK: case DBF_OUTLINK: case DBF_FWDLINK:
    ///     status = dbPutLink(&pscalcout->out, DBR_STRING, &pscalcout->osv, 1);
    /// ```
    ///
    /// [`DbFieldType`] is the port's DBR *wire* type and cannot express
    /// `DBF_MENU` / `DBF_DEVICE` — C's DBF class is not the DBR type. The
    /// classification therefore lives here, with the record's field metadata,
    /// where each class is already known:
    ///
    /// * `DBF_STRING` and the three link classes — the port stores links and
    ///   `DTYP` (C's only `DBF_DEVICE` field) as strings;
    /// * `DBF_ENUM` — an enum-typed field;
    /// * `DBF_MENU` — a menu-index field, i.e. one this record resolves choice
    ///   labels for ([`Self::menu_choices_for`]): `PRIO`, `STAT`, `SEVR`,
    ///   `DISS`, `ACKT`, `SCAN`, `IVOA`, `OMSL`, … The index is stored as a
    ///   short, so a same-named field that is NOT a menu index (scalcout's
    ///   string `OSV` shares a name with the alarm-severity menu) is
    ///   classified by its own type, not by the name collision.
    ///
    /// Everything else (`DBF_DOUBLE`, `DBF_LONG`, `DBF_CHAR`, …) falls to the
    /// device support's `default:` arm.
    ///
    /// The question is about the target field's DECLARED class, so it is asked
    /// of the declaration ([`Self::declared_field_type`]) and not of the
    /// variant the record stores: C's `switch` is on `dbAddr.field_type`, which
    /// `dbNameToAddr` took from the `dbFldDes`. `DBF_MENU` and `DBF_DEVICE` both
    /// map to `DbFieldType::Enum` in the generated tables (`mapDBFToDBR`), and
    /// the three link classes to `DbFieldType::String`, so the seven C arms are
    /// exactly these two.
    pub(crate) fn field_puts_as_string(&self, field: &str) -> bool {
        let Some(declared) = self.declared_field_type(field) else {
            return false;
        };
        matches!(declared, DbFieldType::String | DbFieldType::Enum)
    }

    /// The field's value as C `dbGetLink(plink, DBR_STRING, ...)` delivers it —
    /// the SOURCE side of an input link read with
    /// [`LinkReadAs::String`](super::record_trait::LinkReadAs::String).
    ///
    /// C converts at the source, through `dbConvert.c`'s
    /// `[field_type][DBR_STRING]` table: a `DBF_ENUM` field goes through
    /// `getEnumString` → the record's `get_enum_str` (mbbi's `ZRST`.., bi's
    /// `ZNAM`/`ONAM`) and a `DBF_MENU` field through `getMenuString` → the
    /// menu's choice string, i.e. the state LABEL in both cases, never the
    /// index. Only the record holds those tables, so the render lives here with
    /// the field metadata — the link-read owner has an index and nothing to
    /// resolve it with.
    ///
    /// The render goes through [`Self::enum_string_form_for`], the same owner
    /// the CA/PVA encoders use, so a link read and a `caget -t` of one field can
    /// never disagree about its string.
    pub(crate) fn field_as_dbr_string(&self, field: &str) -> Option<PvString> {
        let value = self.resolve_field(field)?;
        // A `DBF_ENUM` index, and a `DBF_MENU` index (stored as a short),
        // render through the field's string source. A short field that is
        // neither has no source and stays the plain number C converts it to.
        let idx = match value {
            EpicsValue::Enum(v) => Some(v),
            EpicsValue::Short(v) => u16::try_from(v).ok(),
            _ => None,
        };
        if let Some(idx) = idx
            && let Some(form) = self.enum_string_form_for(field)
        {
            return Some(form.render(idx));
        }
        value_as_dbr_string(&value)
    }

    /// The field's declaration — its `dbFldDes`, in C's terms.
    ///
    /// The `.dbd` is the declaration, so the table generated FROM the `.dbd`
    /// ([`dbd_generated::record_fields`](super::dbd_generated::record_fields))
    /// is asked first, for every record type that has one. A record's own
    /// `Record::field_list` is a
    /// hand-written stand-in for that table, and it is consulted only for a
    /// record type the `.dbd` does not cover (`subArray`, and the record types
    /// the downstream crates add). It cannot be the primary answer: several of
    /// those tables are *derived from the record's Rust storage types* — the
    /// `#[derive(EpicsRecord)]` records type `longin.ADEL` `DBF_DOUBLE`
    /// because the struct member is an `f64`, where the `.dbd` says
    /// `DBF_LONG` — and reading the type off the storage is the whole defect
    /// this owner exists to close.
    ///
    /// `dbCommon` last, matching the order [`Self::resolve_field`] reads the
    /// value in, so a record-specific field always shadows the common one in
    /// both halves.
    ///
    /// `None` for a field with no declaration at all: a virtual field
    /// (`RTYP`, `TIME`, ...), which C answers from dbStaticLib rather than
    /// from a `dbFldDes`.
    pub(crate) fn field_desc(&self, field: &str) -> Option<&'static FieldDesc> {
        field_desc_of(self.record.as_ref(), field)
    }

    /// Is `field` (already uppercased) a `DBF_NOACCESS` internal name —
    /// record-own (`BPTR`, `RPVT`, ...) or `dbCommon` (`MLOK`, `RSET`, ...)?
    ///
    /// C's `dbNameToAddr` resolves such a name, so a SEARCH for it is
    /// answered and the refusal lands at channel creation (`mapDBFToDBR` →
    /// `DBR_NOACCESS`). The search gate (`PvDatabase::has_name_no_resolve`)
    /// asks this so the port answers the same way; every value path stays
    /// closed to these names.
    pub(crate) fn resolves_noaccess_name(&self, field: &str) -> bool {
        is_dbcommon_noaccess(field) || self.record.noaccess_names().contains(&field)
    }

    /// The `DBF_*` type `field` is SERVED as — the single source of truth for
    /// the type on the wire, on every delivery path.
    ///
    /// This is the field's DECLARED type ([`FieldDesc::dbf_type`], from the
    /// `.dbd`), not the type of whatever variant the record happens to store.
    /// C resolves a channel's `field_type` from the `dbFldDes` at
    /// name-resolution time (`dbChannelCreate` -> `dbNameToAddr`,
    /// `dbAccess.c:184-205`) and every later `dbGet`/`db_post_events` converts
    /// the stored bytes to it — the storage is private to the record, the
    /// declaration is the contract.
    ///
    /// Two answers are NOT the declaration:
    ///
    /// * a [`FieldDesc::runtime_typed`] field — C's `cvt_dbaddr` overwrites
    ///   `paddr->field_type` from record state (`FTVL`, `FTA`, `SDEF`), and
    ///   this port's `cvt_dbaddr` is the variant the record stores;
    /// * a field with no `FieldDesc` at all (a virtual field).
    ///
    /// In both cases the value's own type is the answer, so this returns
    /// `None` and [`Self::project_to_declared_type`] leaves the value alone.
    pub fn declared_field_type(&self, field: &str) -> Option<DbFieldType> {
        declared_field_type_of(self.record.as_ref(), field)
    }

    /// Project a field's stored value onto its declared type
    /// ([`Self::declared_field_type`]) — the single owner of "what type this
    /// field goes on the wire as", run by the CA create-channel path
    /// ([`Self::client_field_value`]), the GET path
    /// ([`Self::snapshot_for_field`]) and the MONITOR path
    /// ([`Self::make_monitor_snapshot`]), so all three announce and serve the
    /// same type.
    ///
    /// The projection is [`EpicsValue::convert_to`], the one value-coercion
    /// owner — the same routine `dbGet` converts through. Never re-derive a
    /// conversion here: C picks its routine from BOTH the source and the
    /// destination type, and only `convert_to` knows that table.
    ///
    /// Idempotent: a value already of its declared type is short-circuited by
    /// `convert_to`, and re-projecting a projected value is a no-op. That is
    /// what lets the CA path derive the native type from the value it is about
    /// to serve.
    pub fn project_to_declared_type(&self, field: &str, value: EpicsValue) -> EpicsValue {
        match self.declared_field_type(field) {
            Some(declared) => value.convert_to(declared),
            None => value,
        }
    }

    /// The client-facing value of `field`: the resolved value projected onto
    /// the field's declared type ([`Self::project_to_declared_type`]), so a
    /// native type derived from the value — which is what the CA
    /// create-channel path does — is the DECLARED type, and matches the
    /// GET/MONITOR data byte for byte.
    pub fn client_field_value(&self, field: &str) -> Option<EpicsValue> {
        let value = self.resolve_field(field)?;
        Some(self.project_to_declared_type(field, value))
    }

    /// Attach a `DBF_MENU` field's `menu()` choice labels to a built snapshot,
    /// so the CA/PVA enum encoders present `"NO CONVERSION"` rather than `0`.
    ///
    /// The VALUE half of the `DBF_MENU` -> `DBR_ENUM` mapping is not here: the
    /// `.dbd` declares a menu field `DBF_MENU`, the generator types that
    /// `DbFieldType::Enum` (`mapDBFToDBR`), and
    /// [`Self::project_to_declared_type`] — which every delivery path runs —
    /// makes the served value an [`EpicsValue::Enum`] on that declaration
    /// alone. So the label table is all that is left to attach, and it is
    /// attached exactly when the served value came out an enum. A same-named
    /// field that is NOT a menu index (`scalcout.OSV`, declared `DBF_STRING`,
    /// shares a name with the alarm-severity menu) is served as its own
    /// declared string and gets no choice table.
    fn attach_menu_enum(&self, field: &str, snap: &mut super::super::snapshot::Snapshot) {
        if !matches!(snap.value, EpicsValue::Enum(_)) {
            return;
        }
        // `VAL` is the one field whose two rset slots differ: C's
        // `get_enum_strs` (the `DBR_GR_ENUM` labels) is TRIMMED to `no_str`
        // while `get_enum_str` (the DBR_STRING form) indexes the untrimmed
        // state array. `populate_enum_info` owns that pair; every OTHER
        // enum-valued field is a menu or a device, whose one choice list
        // answers both (C `getMenuString`/`getDeviceString` index the same
        // `papChoiceValue` the GR_ENUM reply carries).
        if field.eq_ignore_ascii_case("VAL") {
            return;
        }
        let Some(form) = self.enum_string_form_for(field) else {
            return;
        };
        snap.enums = Some(super::super::snapshot::EnumInfo::with_string_form(
            form.slots.clone(),
            form,
        ));
    }

    /// Build a Snapshot with full metadata for the given field — for a field
    /// **no link backs**.
    ///
    /// A link-backed field answers `None` here on purpose. Its metadata has to
    /// be resolved from the target record, which needs a
    /// [`PvDatabase`](crate::server::database::PvDatabase) and, because the
    /// port has one lock per record instead of C's per-lock-set recursive
    /// mutex, has to happen with no record lock held. That is
    /// [`PvDatabase::channel_snapshot_for_field`](crate::server::database::PvDatabase::channel_snapshot_for_field),
    /// and it is the only entry point that can serve one. Answering `None`
    /// rather than a seeded snapshot is what makes a caller that reached for
    /// the wrong door serve nothing instead of something stale.
    pub fn snapshot_for_field(&self, field: &str) -> Option<super::super::snapshot::Snapshot> {
        if self.link_backed_metadata_field_of(field).is_some() {
            return None;
        }
        self.snapshot_for_field_with(field, LinkBacking::none())
    }

    /// [`Self::snapshot_for_field`] with the link metadata the caller resolved
    /// for this build. `PvDatabase` is the intended caller; see [`LinkBacking`].
    pub fn snapshot_for_field_with(
        &self,
        field: &str,
        backing: LinkBacking<'_>,
    ) -> Option<super::super::snapshot::Snapshot> {
        // The GET path serves the field at its DECLARED type, the same type
        // the CA create-channel path announced from `client_field_value` and
        // the same one the monitor path posts.
        let value = self.client_field_value(field)?;
        Some(self.finish_field_snapshot(field, value, backing))
    }

    /// Which of this record's own link fields, if any, supplies `field`'s
    /// metadata — C's `get_linkNumber` question, asked before any lock is
    /// dropped so `PvDatabase` knows whether it has to resolve at all.
    pub(crate) fn link_backed_metadata_field_of(&self, field: &str) -> Option<String> {
        self.record
            .link_backed_metadata_field(&field.to_ascii_uppercase())
    }

    /// The value a channel bound to `field` serves, through the `$` view
    /// the channel was bound with.
    ///
    /// `dbChannelCreate` decides the view ONCE, at bind time
    /// (`dbChannel.c:486-505`), and every delivery path then reads through
    /// the `dbChannel` it produced; this is that single read. Callers must
    /// not re-derive it: resolving the bare field name answers "yes" for
    /// `VAL` whatever its type, so a path that does drops the eligibility
    /// half of the view entirely and admits `REC.VAL$` on a `DBF_DOUBLE`.
    ///
    /// `None` is `S_dbLib_fieldNotFound`: the record has no such field, or
    /// `$` was applied to a field that cannot be re-viewed as a character
    /// array (see [`Self::resolve_string_view_field`]).
    pub fn channel_field_value(&self, field: &str, string_view: bool) -> Option<EpicsValue> {
        if string_view {
            self.resolve_string_view_field(field)
        } else {
            self.client_field_value(field)
        }
    }

    /// [`Self::snapshot_for_field_with`] through the same `$` view as
    /// [`Self::channel_field_value`] — the metadata is the field's either
    /// way, only the value is re-viewed.
    ///
    /// This is the `_with` variant deliberately: the view decides the VALUE,
    /// `backing` decides the METADATA, and the two are independent. A caller
    /// that has resolved a [`LinkBacking`] passes it straight through, so a
    /// link-backed `$` member keeps its target's units/precision.
    pub fn channel_snapshot_for_field(
        &self,
        field: &str,
        string_view: bool,
        backing: LinkBacking<'_>,
    ) -> Option<super::super::snapshot::Snapshot> {
        let value = self.channel_field_value(field, string_view)?;
        Some(self.finish_field_snapshot(field, value, backing))
    }

    /// The one finishing pipeline behind both `Snapshot` producers
    /// ([`Self::snapshot_for_field`] for GET, [`Self::make_monitor_snapshot`]
    /// for updates). Every step that shapes a served snapshot — alarm/utag
    /// carry, the metadata cache, per-field routing and RSET overrides, menu
    /// enums, property support, the `Q:time:tag` nsec split — runs here, so
    /// the two paths cannot drift apart. Upstream pvxs PR #189 is exactly
    /// that drift: its subscription callback served unmasked nanoseconds
    /// while its GET path applied the nsec mask.
    fn finish_field_snapshot(
        &self,
        field: &str,
        value: EpicsValue,
        backing: LinkBacking<'_>,
    ) -> super::super::snapshot::Snapshot {
        let mut snap = super::super::snapshot::Snapshot::new(
            value,
            self.common.stat,
            self.common.sevr as u16,
            self.common.time,
        );
        // Default the served `timeStamp.userTag` to the record's `utag`,
        // mirroring pvxs `iocsource.cpp:245` (`auto utag = meta.utag;`).
        // The 64-bit `epicsUTag` narrows to the int32 NT wire field by
        // truncating to the low 32 bits — pvxs assigns the same uint64
        // straight into the `Int32` `timeStamp.userTag`. The `Q:time:tag`
        // nsec-LSB split below overrides this when configured, matching
        // pvxs `if(info.nsecMask) utag = meta.time.nsec & info.nsecMask;`
        // (:246-247 — the test and its assignment).
        snap.user_tag = self.common.utag as i32;
        // Carry the record's committed alarm message (`common.amsg`) so a
        // PVA read serves `alarm.message` from the record's own amsg
        // (pvxs `iocsource.cpp:230-236` prefers `meta.amsg`) rather than a
        // string re-synthesized from the condition code. Empty for records
        // that raise no message (C's plain `recGblSetSevr` clears namsg).
        snap.alarm.amsg = self.common.amsg.clone();

        // Pull display/control/enums from the metadata cache (build on
        // first call, hit thereafter until invalidated by a metadata-class
        // field write).
        let meta = self.cached_metadata();
        snap.display = meta.display;
        snap.control = meta.control;
        snap.enums = meta.enums;

        // The cache above is the record's VAL metadata. C routes PER FIELD, so
        // a non-VAL-class field does NOT get VAL's limits — see
        // [`Self::route_field_metadata`], which owns that decision.
        self.route_field_metadata(field, backing, &mut snap);

        // Per-field RSET metadata (C get_units/get_precision/
        // get_graphic_double/get_control_double/get_alarm_double key on
        // dbGetFieldIndex) patches the record-level cache for this field.
        self.apply_field_metadata_override(field, &mut snap);

        // DBF_MENU field (a shared menu such as `SCAN`/`OMSL`/`HHSV`/... or
        // a record-specific menu such as `sel.SELM`): carry the menu index
        // as DBR_ENUM and attach its `menu()` choice labels. See
        // `attach_menu_enum`. This overrides any record VAL enum table
        // copied from the metadata cache above, because a menu field
        // carries its own menu's choices, not the record's VAL state
        // strings.
        self.attach_menu_enum(field, &mut snap);

        // The metadata VALUES and the mask that says which of them this
        // channel actually supplies are assigned by the same owner, from the
        // settled value, so they cannot disagree.
        self.assign_property_support(field, &mut snap);

        // apply `info(Q:time:tag, "nsec:lsb:N")` — pvxs
        // `iocsource.cpp:239-248` publishes `nanoseconds & ~nsecMask` and
        // moves `nanoseconds & nsecMask` into `timeStamp.userTag`. The
        // split is applied to both `snap.timestamp` and `snap.user_tag` so
        // downstream encoders (NTScalar `timeStamp`, QSRV groups) all see
        // the same shape. A zero mask (tag absent or unparseable) is a
        // no-op inside the helper, exactly as pvxs's `if(info.nsecMask)`
        // gate is.
        crate::server::snapshot::apply_nsec_mask(&mut snap, self.qtime_nsec_mask());

        snap
    }

    /// Resolve `info(Q:time:tag)` to pvxs's `MappingInfo::nsecMask`.
    /// Returns 0 (the "no split" mask) when the tag is absent or does not
    /// parse — pvxs leaves `nsecMask` at its 0 initialiser in that case.
    ///
    /// pvxs `ioc/typeutils.cpp:79-88`:
    ///
    /// ```c
    /// if(auto val = ent.info("Q:time:tag")) {
    ///     epicsInt32 dig = 0;
    ///     if(strncmp(val, "nsec:lsb:", 9)==0 && !epicsParseInt32(&val[9], &dig, 10, nullptr)) {
    ///         nsecMask = (uint64_t(1u)<<dig)-1u;
    ///     }
    /// }
    /// ```
    ///
    /// The prefix test is a byte-exact `strncmp` — no case folding and no
    /// whitespace tolerance, so `NSEC:LSB:4` and `nsec: lsb: 4` do NOT
    /// match and leave the timestamp alone. There is no bounds clamp
    /// either: any `dig` `epicsParseInt32` accepts is shifted verbatim, so
    /// `nsec:lsb:31` yields the `0x7FFF_FFFF` mask pvxs actually serves.
    fn qtime_nsec_mask(&self) -> u64 {
        let Some(rest) = self
            .get_info("Q:time:tag")
            .and_then(|v| v.strip_prefix("nsec:lsb:"))
        else {
            return 0;
        };
        let Some(dig) = epics_parse_int32_base10(rest) else {
            return 0;
        };
        // C shifts `uint64_t(1u)` by an `epicsInt32`. A `dig` outside
        // `0..=63` is UB in C++; every ISA EPICS builds on (x86-64 `shlq`,
        // aarch64 `lsl`) takes the shift count modulo 64, which is what
        // `wrapping_shl` does — so `nsec:lsb:64` disables the split
        // (mask 0) and a negative `dig` shifts by `dig & 63`, the same
        // masks pvxs produces on those hosts.
        1u64.wrapping_shl(dig as u32) - 1
    }

    /// Populate DisplayInfo from record fields if applicable.
    /// Resolve the `Q:form` info-tag value to a `display.form` menu index.
    ///
    /// pvxs publishes the fixed seven-entry form menu
    /// (Default/String/Binary/Decimal/Hex/Exponential/Engineering) for every
    /// numeric value and, for the VAL field only, sets `display.form.index`
    /// to the slot whose name equals the field's `Q:form` info tag
    /// (`iocsource.cpp:42-62`, case-sensitive). Unset or unrecognised ->
    /// `None` (form stays 0 = Default), exactly as pvxs leaves the index
    /// untouched on no match.
    fn q_form_index(&self) -> Option<i16> {
        const FORM_NAMES: [&str; 7] = [
            "Default",
            "String",
            "Binary",
            "Decimal",
            "Hex",
            "Exponential",
            "Engineering",
        ];
        let tag = self.info.get("Q:form")?;
        FORM_NAMES
            .iter()
            .position(|name| name == tag)
            .map(|i| i as i16)
    }

    /// Stamp a built snapshot with the property mask THIS channel supplies —
    /// [`Record::property_support`] narrowed to the addressed field by C's
    /// second gate ([`PropertySupport::narrowed_to_field`]). Called by both
    /// snapshot builders once the value has settled (after
    /// [`Self::attach_menu_enum`] promoted a `DBF_MENU` field to its
    /// `DBR_ENUM` form), so the mask is read off the same value the client
    /// receives and no consumer has to re-derive either gate.
    fn assign_property_support(&self, field: &str, snap: &mut super::super::snapshot::Snapshot) {
        snap.properties = self.record.property_support().narrowed_to_field(
            snap.value.db_field_type(),
            self.menu_choices_for(field).is_some(),
        );
    }

    /// The property mask a channel on `field` supplies, without building a
    /// snapshot — what a PVA server needs to decide which NT leaves it may
    /// MARK for a channel it has not read yet (QSRV resolves a group's member
    /// masks once, at monitor start, rather than per event).
    ///
    /// Same two gates, same owner as `Self::assign_property_support`: an
    /// unknown field supplies nothing.
    pub fn property_support_for_field(&self, field: &str) -> PropertySupport {
        let Some(value) = self.client_field_value(field) else {
            return PropertySupport::NONE;
        };
        self.record.property_support().narrowed_to_field(
            value.db_field_type(),
            self.menu_choices_for(field).is_some(),
        )
    }

    /// The record-level display metadata cache: units, precision and display
    /// limits as C's `get_units` / `get_precision` / `get_graphic_double`
    /// answer them for the fields their rset lists.
    ///
    /// Driven by [`Record::property_support`], not by a `match` on the record
    /// type. Those were two independent tables answering the same question,
    /// and nothing kept them in step: `default_property_support` declares
    /// units and precision for twenty-five record types where the arm list
    /// here covered nine, so the other sixteen declared the leaf and served
    /// `""` / `0` — `sel`, `sub`, `dfanout`, `subArray`, `scalcout`,
    /// `acalcout`, `epid`, `scaler`, `swait`, `sseq`, `seq`, `mca`,
    /// `histogram`, `transform`, `throttle` and `asyn`. Deriving the cache
    /// from the declaration makes "declares the slot" and "supplies the slot"
    /// one fact rather than two that can disagree.
    ///
    /// Precision matters well beyond `caget -d`: it is also what the
    /// DBF_DOUBLE to DBR_STRING conversion renders with, in C
    /// (`dbConvert.c:783-786` calls `prset->get_precision` with no field-type
    /// gate) and here (`codec.rs::convert_value_to_dbr_string`), so a missing
    /// slot changed the digits of a plain `caget`.
    ///
    /// The sources are the same in every C rset that supplies them — `EGU` for
    /// `get_units` and `PREC` for `get_precision` — so only the graphic pair
    /// needs a per-type table (`graphic_limit_fields`). Per-FIELD departures
    /// from the record's own values stay where C puts them, in that record's
    /// [`Record::field_metadata_override`], which is applied after this and
    /// wins.
    fn populate_display_info(&self, snap: &mut super::super::snapshot::Snapshot) {
        let slots = self.record.property_support();
        if slots.units || slots.precision || slots.graphic_double {
            let (upper, lower) = if slots.graphic_double {
                let (hi, lo) = super::record_trait::graphic_limit_fields(self.record.record_type());
                (self.metadata_limit(hi), self.metadata_limit(lo))
            } else {
                (0.0, 0.0)
            };
            snap.display = Some(super::super::snapshot::DisplayInfo {
                units: if slots.units {
                    self.metadata_units()
                } else {
                    Default::default()
                },
                precision: if slots.precision {
                    self.metadata_limit("PREC") as i16
                } else {
                    0
                },
                upper_disp_limit: upper,
                lower_disp_limit: lower,
                ..Default::default()
            });
        }
        // Apply the `Q:form` display-format hint. The block above builds
        // `snap.display` for every record type that supplies at least one
        // display slot — the same set for which pvxs emits
        // `display.form.choices`. This cache is record-level (it is the VAL
        // field's metadata); the VAL-only rule pvxs applies to
        // `display.form.index` (`iocsource.cpp:53`) is enforced per served
        // field in `apply_field_metadata_override`.
        if let Some(display) = snap.display.as_mut() {
            if let Some(form) = self.q_form_index() {
                display.form = form;
            }
        }
        // `display.description` from dbCommon DESC — pvxs QSRV fills it
        // on every metadata populate (iocsource.cpp:306-310), for every
        // record type including those with no other display source. The
        // qsrv builders always emit the leaf (defaulting a `None`
        // display), so creating the DisplayInfo here changes leaf
        // values, never the wire shape. Cache freshness is owned by the
        // DESC arm of `put_common_field`, which invalidates without
        // posting DBE_PROPERTY (epics-base#785 / UI-106).
        snap.display
            .get_or_insert_with(Default::default)
            .description = self.common.desc.clone();
    }

    /// The record-level control-limit cache — what C's `get_control_double`
    /// answers for the fields its rset lists.
    ///
    /// Gated on the declared slot for the same reason as
    /// [`Self::populate_display_info`], and with the same effect: the arm list
    /// this replaces covered thirteen record types where
    /// `default_property_support` declares `control_double` for twenty-three,
    /// so `sel`, `sub`, `dfanout`, `subArray`, `histogram`, `scalcout`,
    /// `acalcout` and `epid` served 0/0 on `VAL` — a channel whose C rset
    /// answers the record's own operator range.
    ///
    /// Which of the record's fields that range comes from is the one thing
    /// that varies by type, and it lives in `control_limit_source`.
    fn populate_control_info(&self, snap: &mut super::super::snapshot::Snapshot) {
        use super::record_trait::ControlLimitSource;

        if !self.record.property_support().control_double {
            return;
        }
        let (upper, lower) =
            match super::record_trait::control_limit_source(self.record.record_type()) {
                ControlLimitSource::Drive => {
                    (self.metadata_limit("DRVH"), self.metadata_limit("DRVL"))
                }
                ControlLimitSource::DriveWhenSet => {
                    let (drvh, drvl) = (self.metadata_limit("DRVH"), self.metadata_limit("DRVL"));
                    if drvh > drvl {
                        (drvh, drvl)
                    } else {
                        (self.metadata_limit("HOPR"), self.metadata_limit("LOPR"))
                    }
                }
                ControlLimitSource::SoftLimits => {
                    (self.metadata_limit("HLM"), self.metadata_limit("LLM"))
                }
                ControlLimitSource::Operator => {
                    (self.metadata_limit("HOPR"), self.metadata_limit("LOPR"))
                }
            };
        snap.control = Some(super::super::snapshot::ControlInfo {
            upper_ctrl_limit: upper,
            lower_ctrl_limit: lower,
        });
    }

    /// A numeric metadata field (`PREC`, `HOPR`, `DRVH`, ...) as C reads it —
    /// straight out of record memory, which for C means every field the `.dbd`
    /// declares.
    ///
    /// Through [`Self::resolve_field`], NOT `Record::get_field`, and that is
    /// the whole reason this cache used to read zero for the types it now
    /// serves: a Rust record implements only the fields it has behaviour for,
    /// so `sel`, `sub`, `dfanout` and their siblings model no `PREC`/`HOPR`
    /// cell at all and `get_field` answers `None` for them. Their `.db` values
    /// live in `declared_overrides`, and their unset defaults in the `.dbd`
    /// `initial()`, both of which only `resolve_field` reaches.
    fn metadata_limit(&self, field: &str) -> f64 {
        self.resolve_field(field)
            .and_then(|v| v.to_f64())
            .unwrap_or(0.0)
    }

    /// `EGU`, the source every ported C `get_units` copies from. Empty for a
    /// record type whose `.dbd` declares no `EGU` — `seq` and `histogram` have
    /// none, and C writes nothing into the `dbAccess.c:378` seed for either.
    fn metadata_units(&self) -> crate::types::PvString {
        match self.resolve_field("EGU") {
            Some(EpicsValue::String(s)) => s,
            _ => Default::default(),
        }
    }

    /// Whether C's `get_units` copies the record's own `EGU` into `field`.
    ///
    /// The fourth membership question, and the one the port never asked. Units
    /// had no per-field step at all: the record-level cache was the entire
    /// answer, so every field of a type that supplies the slot was served
    /// `EGU` — including the fields whose C rset tests first and writes
    /// nothing, leaving the `dbAccess.c:378` empty seed. Measured shape:
    /// `caget -d DBR_GR_DOUBLE AI.SMOO` served `EGU` where `aiRecord.c:223-226`
    /// deliberately skips the three raw-conversion fields.
    ///
    /// * `ai`/`ao` (`aiRecord.c:217-232`, `aoRecord.c:284-298`) — a DBF_DOUBLE
    ///   field other than the raw-conversion ones, which carry no engineering
    ///   units.
    /// * `calc`/`calcout`/`sub`/`sel`/`dfanout` (`calcRecord.c:169-182`,
    ///   `calcoutRecord.c:425-444`, `subRecord.c:206-219`,
    ///   `selRecord.c:136-143`, `dfanoutRecord.c:155-163`) — any DBF_DOUBLE
    ///   field.
    /// * `longin`/`longout` (`longinRecord.c:183-191`) test DBF_LONG and the
    ///   int64 pair (`int64inRecord.c:179-187`) DBF_INT64: the record's own VAL
    ///   type, not DOUBLE.
    /// * `compress` (`compressRecord.c:449-458`) widens the DBF_DOUBLE test
    ///   with `VAL`, whose served type comes from the record rather than the
    ///   dbd.
    /// * the array types (`waveformRecord.c:220-233`, `aaiRecord.c`,
    ///   `aaoRecord.c`, `subArrayRecord.c:202-215`) name `VAL`, `HOPR` and
    ///   `LOPR`, and drop `VAL` when `FTVL` makes it strings or enums.
    /// * `histogram`, `seq`, `bo`, `table` and `aSub` never write `EGU` at all;
    ///   each answers a literal or a link for a named set and nothing
    ///   elsewhere, and the literals come from
    ///   [`Record::field_metadata_override`].
    ///
    /// Every other ported type copies `EGU` with no test whatever
    /// (`sCalcoutRecord.c:603-609`, `aCalcoutRecord.c:743-749`,
    /// `epidRecord.c:217-223`, `mcaRecord.c:884-890`, `motorRecord.cc`'s
    /// `default:` arm).
    fn units_from_egu(&self, rtype: &str, field: &str) -> bool {
        use crate::types::DbFieldType as T;
        let f = field.to_ascii_uppercase();
        // The link arm is NOT here: `route_field_metadata` asks
        // [`Record::link_backed_metadata_field`] first and only falls through
        // to this EGU question for a field no link backs. This function
        // answers C's `else strncpy(units, prec->egu, ...)` branch alone.
        let own = |t: T| self.static_field_type(&f) == Some(t);
        match rtype {
            "ai" => own(T::Double) && !matches!(f.as_str(), "ASLO" | "AOFF" | "SMOO"),
            "ao" => own(T::Double) && !matches!(f.as_str(), "ASLO" | "AOFF"),
            "calc" | "calcout" | "sub" | "sel" | "dfanout" => own(T::Double),
            "longin" | "longout" => own(T::Long),
            "int64in" | "int64out" => own(T::Int64),
            "compress" => own(T::Double) || f == "VAL",
            "waveform" | "aai" | "aao" | "subArray" => {
                matches!(f.as_str(), "HOPR" | "LOPR")
                    || (f == "VAL" && !self.ftvl_is_string_or_enum())
            }
            "histogram" | "seq" | "bo" | "table" | "aSub" => false,
            _ => true,
        }
    }

    /// `FTVL` names `DBF_STRING` or `DBF_ENUM` — the two element types for
    /// which the array rsets break out of the `VAL` case before copying `EGU`.
    /// `menuFtype` is declared in `DBF_` code order, so `0` is STRING and `11`
    /// is ENUM.
    fn ftvl_is_string_or_enum(&self) -> bool {
        matches!(
            self.resolve_field("FTVL").and_then(|v| v.to_f64()),
            Some(x) if x == 0.0 || x == 11.0
        )
    }

    /// Populate EnumInfo — C rset `get_enum_strs`.
    ///
    /// The table comes from [`Record::enum_state_strings`], the SAME slot the
    /// string-put converter (`dbConvert.c::putStringEnum`) resolves against, so
    /// the choice list a client reads and the names it may write are one table
    /// by construction. It arrives already trimmed to C's `no_str` (bi/bo/busy
    /// drop an empty ONAM behind a set ZNAM — `boRecord.c:342-352`; mbbi/mbbo
    /// cut at the last non-empty state — `mbbiRecord.c:262-269`).
    ///
    /// The `DBR_STRING` half of the same channel is the record's OTHER rset slot
    /// (`get_enum_str`), which is not this trimmed list — see
    /// [`EnumStringForm`]. A record that has no such slot (every record but
    /// bi/bo/busy/mbbi/mbbo, including the downstream crates' own enum records)
    /// renders from its label list, which is what C's absent slot amounts to.
    fn populate_enum_info(&self, snap: &mut super::super::snapshot::Snapshot) {
        if let Some(strings) = self.record.enum_state_strings() {
            snap.enums = Some(match self.record.enum_string_form() {
                Some(form) => super::super::snapshot::EnumInfo::with_string_form(strings, form),
                None => super::super::snapshot::EnumInfo::new(strings),
            });
        }
    }

    /// Get a common field value.
    pub fn get_common_field(&self, name: &str) -> Option<EpicsValue> {
        match name {
            "SEVR" => Some(EpicsValue::Short(self.common.sevr as i16)),
            "STAT" => Some(EpicsValue::Short(self.common.stat as i16)),
            "NSEV" => Some(EpicsValue::Short(self.common.nsev as i16)),
            "NSTA" => Some(EpicsValue::Short(self.common.nsta as i16)),
            // epics-base PR #568 / #566 — alarm message string.
            "AMSG" => Some(EpicsValue::String(self.common.amsg.clone().into())),
            "NAMSG" => Some(EpicsValue::String(self.common.namsg.clone().into())),
            "ACKS" => Some(EpicsValue::Short(self.common.acks as i16)),
            // `ACKT` and `PINI` are `DBF_MENU` (`menuYesNo` /`menuPini`,
            // `dbCommon.dbd.pod:335,169`), not `DBF_UCHAR`: they carry a menu
            // index, which `promote_menu_value` lifts to `DBR_ENUM` with the
            // menu's choice strings. Storing them as `Short` is what makes
            // them eligible for that promotion — see `promote_menu_value`.
            "ACKT" => Some(EpicsValue::Short(if self.common.ackt { 1 } else { 0 })),
            // DBF_UCHAR: served as UChar (declared type) so the raw put byte
            // round-trips to the wire — see the DISP/TPRO comment above.
            "UDF" => Some(EpicsValue::UChar(self.common.udf)),
            "UDFS" => Some(EpicsValue::Short(self.common.udfs)),
            "SCAN" => Some(EpicsValue::Enum(self.common.scan.to_u16())),
            "SSCN" => Some(EpicsValue::Enum(self.common.sscn.to_u16())),
            // `OLDSIMM` is `DBF_MENU`/`menu(menuSimm)`, stored as the menu index
            // and promoted to `DBR_ENUM` with the NO/YES/RAW labels by
            // `promote_menu_value` (shared registry — the saved copy is ALWAYS
            // menuSimm, unlike the live SIMM). Written only by the simulation
            // owner (`rec_gbl_save_simm`); `special(SPC_NOMOD)` for clients.
            "OLDSIMM" => Some(EpicsValue::Short(self.common.oldsimm)),
            "PINI" => Some(EpicsValue::Short(self.common.pini)),
            // DISP/TPRO/RPRO/UDF are `DBF_UCHAR` in `dbCommon.dbd`. Serve them
            // as `UChar` — their DECLARED type — so `project_to_declared_type`
            // is identity and the raw put byte reaches the wire untouched (C
            // stores the byte and `caget` renders `DBR_CHAR` signed: 255 → -1).
            // Serving `Char` here instead routed the value through the lossy
            // `Char → UChar` projection (signed −1 clamped to 0), so a
            // `caput DISP 255` read back as 0 rather than C's -1. `BKPT` is
            // `DBF_NOACCESS`: no `FieldDesc`, no projection, served `Char`.
            "TPRO" => Some(EpicsValue::UChar(self.common.tpro)),
            "BKPT" => Some(EpicsValue::Char(self.common.bkpt)),
            "FLNK" => Some(EpicsValue::String(self.common.flnk.clone().into())),
            // A record type whose C `.dbd` has no INP has no `.INP` channel
            // either — C's dbChannel resolution is the dbd, so `dbgf HI.INP` on
            // a histogram answers "PV 'HI.INP' not found". The port keeps INP on
            // `CommonFields` for every record, so `declares_inp_link()` is what
            // stands in for the dbd, and it must gate the read side as well as
            // the write side (`put_common_field`) — otherwise the field is
            // unloadable and unwritable yet still resolves as a channel.
            "INP" if self.record.declares_inp_link() => {
                Some(EpicsValue::String(self.common.inp.clone().into()))
            }
            "OUT" => Some(EpicsValue::String(self.common.out.clone().into())),
            // C's DTYP is `DBF_DEVICE`: an epicsEnum16 index into the record
            // type's device menu, NOT the name. The name is what this port
            // stores and dispatches on; the index is what the wire carries.
            "DTYP" => Some(EpicsValue::Enum(self.dtyp_index())),
            "TSE" => Some(EpicsValue::Short(self.common.tse)),
            "TSEL" => Some(EpicsValue::String(self.common.tsel.clone().into())),
            // C `UTAG` is DBF_UINT64 — exposed natively as the unsigned
            // 64-bit value variant so values above i64::MAX round-trip.
            "UTAG" => Some(EpicsValue::UInt64(self.common.utag)),
            "ASG" => Some(EpicsValue::String(self.common.asg.clone().into())),
            "ASL" => Some(EpicsValue::Char(self.common.asl)),
            "DESC" => Some(EpicsValue::String(self.common.desc.clone())),
            "PHAS" => Some(EpicsValue::Short(self.common.phas)),
            "EVNT" => Some(EpicsValue::String(self.common.evnt.clone().into())),
            "PRIO" => Some(EpicsValue::Short(self.common.prio)),
            "DISV" => Some(EpicsValue::Short(self.common.disv)),
            "DISA" => Some(EpicsValue::Short(self.common.disa)),
            "SDIS" => Some(EpicsValue::String(self.common.sdis.clone().into())),
            "DISS" => Some(EpicsValue::Short(self.common.diss)),
            "HYST" => Some(EpicsValue::Double(self.common.hyst)),
            "LCNT" => Some(EpicsValue::Short(self.common.lcnt)),
            "DISP" => Some(EpicsValue::UChar(self.common.disp)),
            "PUTF" => Some(EpicsValue::Char(if self.common.putf { 1 } else { 0 })),
            "RPRO" => Some(EpicsValue::UChar(self.common.rpro)),
            "PACT" => Some(EpicsValue::Char(if self.is_processing() { 1 } else { 0 })),
            // C `dbCommon.dbd`: `field(PROC,DBF_UCHAR)` — the raw put byte is
            // retained in `prec->proc` and served back SIGNED as `DBR_CHAR`
            // (`caput PROC 255` → `caget` = -1), exactly like DISP/RPRO. The
            // `pp(TRUE)` force-process is orthogonal: writing PROC still
            // reprocesses the record (put-path intercept), but the byte sticks.
            "PROC" => Some(EpicsValue::UChar(self.common.proc_field)),
            // Analog alarm fields
            "HIHI" => self
                .common
                .analog_alarm
                .as_ref()
                .map(|a| a.hihi.to_epics_value()),
            "HIGH" => self
                .common
                .analog_alarm
                .as_ref()
                .map(|a| a.high.to_epics_value()),
            "LOW" => self
                .common
                .analog_alarm
                .as_ref()
                .map(|a| a.low.to_epics_value()),
            "LOLO" => self
                .common
                .analog_alarm
                .as_ref()
                .map(|a| a.lolo.to_epics_value()),
            "HHSV" => self
                .common
                .analog_alarm
                .as_ref()
                .map(|a| EpicsValue::Short(a.hhsv)),
            "HSV" => self
                .common
                .analog_alarm
                .as_ref()
                .map(|a| EpicsValue::Short(a.hsv)),
            "LSV" => self
                .common
                .analog_alarm
                .as_ref()
                .map(|a| EpicsValue::Short(a.lsv)),
            "LLSV" => self
                .common
                .analog_alarm
                .as_ref()
                .map(|a| EpicsValue::Short(a.llsv)),
            // swait OUTN is aliased to common.out
            "OUTN" => {
                if self.record.record_type() == "swait" {
                    Some(EpicsValue::String(self.common.out.clone().into()))
                } else {
                    None
                }
            }
            _ => None,
        }
    }

    /// `true` when the record type declares `name` in its own `field_list`,
    /// i.e. the record stores the field itself and owns whatever behaviour
    /// hangs off it.
    ///
    /// This separates the two meanings a link field carries. `common.inp` /
    /// `common.out` is the link *text* — always the value the `.db` file
    /// wrote, for every record type, because C device support reads
    /// `prec->inp` / `prec->out` at `init_record` no matter which layer owns
    /// the field ([`crate::server::db_loader::apply_fields`] keeps it
    /// populated). `parsed_inp` / `parsed_out` is the *framework's* dispatch
    /// of that link, and is armed only for a record type that does NOT
    /// declare the field: a record that declares it drives the link itself
    /// (`multi_output_links` for `acalcout`/`scalcout`, device support for
    /// `motorRecord`/`scalerRecord`, or its own `process`). Arming the
    /// framework path for those too would write the link twice per cycle.
    fn record_declares_field(&self, name: &str) -> bool {
        self.record.implements_field(name)
    }

    /// Set a common field value from a runtime `dbPut` (CA/PVA/`dbpf`/link).
    /// Returns what scan index changes are needed.
    ///
    /// A `DBF_MENU` common field's string is converted by C's runtime
    /// converter, `dbConvert.c::putStringMenu` — see `MenuBound::DbPut`.
    pub fn put_common_field(
        &mut self,
        name: &str,
        value: EpicsValue,
    ) -> CaResult<CommonFieldPutResult> {
        self.put_common_field_bounded(name, value, MenuBound::DbPut)
    }

    /// **The single owner of a record's SCAN transition** — C `dbPutField` on
    /// SCAN, which is `scanDelete(precord)` … `scanAdd(precord)`
    /// (`dbAccess.c::dbPutSpecial` SPC_SCAN, dbScan.c:236-248).
    ///
    /// Two callers reach it, and they are the two C sites that move a record
    /// between scan lists: a `SCAN` put ([`Self::put_common_field`]) and the
    /// simulation-mode scan swap (`recGblCheckSimm`, recGbl.c:427-437, which
    /// calls exactly the same `scanDelete`/`scanAdd` pair). Returns the delta
    /// for the scan-index owner (`PvDatabase::update_scan_index`) to apply once
    /// the record lock is down; [`CommonFieldPutResult::NoChange`] when the scan
    /// did not move.
    pub fn set_scan(&mut self, new_scan: ScanType) -> CommonFieldPutResult {
        let old_scan = self.common.scan;
        self.common.scan = new_scan;
        if old_scan == new_scan {
            return CommonFieldPutResult::NoChange;
        }
        // C `scanDelete`/`scanAdd` call the record's device support
        // `get_ioint_info(1)` / `get_ioint_info(0)`. Only a change of I/O Intr
        // *membership* reaches those; a Passive→"1 second" move calls neither.
        let was_io_intr = old_scan == ScanType::IoIntr;
        let is_io_intr = new_scan == ScanType::IoIntr;
        if was_io_intr != is_io_intr {
            self.record.set_io_intr_scan(is_io_intr);
        }
        CommonFieldPutResult::ScanChanged {
            old_scan,
            new_scan,
            phas: self.common.phas,
        }
    }

    /// C `recGblSaveSimm` (`recGbl.c:421-425`) — latch the CURRENT simulation
    /// mode into OLDSIMM:
    ///
    /// ```c
    /// void recGblSaveSimm(const epicsEnum16 sscn,
    ///     epicsEnum16 *poldsimm, const epicsEnum16 simm) {
    ///     if (sscn == USHRT_MAX) return;
    ///     *poldsimm = simm;
    /// }
    /// ```
    ///
    /// **The only writer of `CommonFields::oldsimm`.** Must run BEFORE the SIMM
    /// value moves — C calls it from `special(SPC_MOD)` pass 0 (before the put)
    /// and from `recGblGetSimm`/`recGblInitSimm` before the SIML read. The
    /// `sscn == 65535` guard is C's: with SSCN unset there is no scan to swap
    /// to, so the latch is not even taken (and [`Self::rec_gbl_check_simm`]
    /// bails on the same test, so the stale OLDSIMM is never read).
    ///
    /// A record type with no SSCN/OLDSIMM in its C dbd (`busy`, `swait`) passes
    /// neither pointer to any recGbl helper: no-op here.
    pub fn rec_gbl_save_simm(&mut self) {
        if !self.record.uses_recgbl_simm_helpers() {
            return;
        }
        // C `recGblSaveSimm`: `if (*psscn == USHRT_MAX) return;` — the literal
        // sentinel, not "any index outside the menu".
        if self.common.sscn.is_unset() {
            return;
        }
        if let Some(EpicsValue::Short(simm)) = self.record.get_field("SIMM") {
            self.common.oldsimm = simm;
        }
    }

    /// C `recGblCheckSimm` (`recGbl.c:427-437`) — on a SIMM transition, swap the
    /// record's SCAN with SSCN:
    ///
    /// ```c
    /// void recGblCheckSimm(struct dbCommon *pcommon, epicsEnum16 *psscn,
    ///     const epicsEnum16 oldsimm, const epicsEnum16 simm) {
    ///     if (*psscn == USHRT_MAX) return;
    ///     if (simm != oldsimm) {
    ///         epicsUInt16 scan = pcommon->scan;
    ///         scanDelete(pcommon);
    ///         pcommon->scan = *psscn;
    ///         scanAdd(pcommon);
    ///         *psscn = scan;
    ///     }
    /// }
    /// ```
    ///
    /// This is what makes SSCN mean anything at all: a record configured
    /// `field(SCAN,"1 second") field(SSCN,"Passive")` stops periodic scanning
    /// the moment SIMM leaves NO, and resumes it when SIMM goes back — with the
    /// two fields having traded places each time. Both are a genuine swap, not
    /// an assignment: SSCN ends up holding the scan the record just left.
    ///
    /// **The only writer of the SIMM-driven SCAN/SSCN swap.** The scan-list
    /// move itself goes through the single SCAN owner [`Self::set_scan`], whose
    /// [`CommonFieldPutResult`] the caller hands to
    /// `PvDatabase::update_scan_index` once the record lock is down. Runs AFTER
    /// the SIMM value moved — C `special(SPC_MOD)` pass 1, and the tail of
    /// `recGblGetSimm`/`recGblInitSimm`.
    pub fn rec_gbl_check_simm(&mut self) -> CommonFieldPutResult {
        if !self.record.uses_recgbl_simm_helpers() {
            return CommonFieldPutResult::NoChange;
        }
        let Some(sim_scan) = self.common.sscn.scan() else {
            // `*psscn == USHRT_MAX` — SSCN unset, no swap. An SSCN that is
            // merely ILLEGAL still swaps: C assigns it into SCAN and `scanAdd`
            // then declines to scan the record.
            return CommonFieldPutResult::NoChange;
        };
        let Some(EpicsValue::Short(simm)) = self.record.get_field("SIMM") else {
            return CommonFieldPutResult::NoChange;
        };
        if simm == self.common.oldsimm {
            return CommonFieldPutResult::NoChange;
        }
        let previous_scan = self.common.scan;
        let result = self.set_scan(sim_scan);
        self.common.sscn = SimModeScan::from_scan(previous_scan);
        result
    }

    /// C `dbAccess.c::putAckt` (`:1285-1300`) — the **only** writer of ACKT.
    ///
    /// Reached from `dbPut` for a `DBR_PUT_ACKT` request *type*
    /// (`dbAccess.c:1331-1332`), ABOVE the `SPC_NOMOD` gate that refuses every
    /// ordinary put to the field. Posts exactly what C posts: the ACKT change,
    /// the ACKS it may lower, and the record-wide `DBE_ALARM` — and only when
    /// `ackt` actually changed (C returns 0 early otherwise).
    pub fn put_ackt(&mut self, value: u16, backing: LinkBacking<'_>) {
        let new_ackt = value != 0;
        if new_ackt == self.common.ackt {
            return;
        }
        use crate::server::recgbl::EventMask;
        let ack_mask = EventMask::VALUE | EventMask::ALARM;
        self.common.ackt = new_ackt;
        self.cleanup_subscribers();
        self.notify_field_backed("ACKT", ack_mask, backing);
        // C `:1294-1297`: turning transient acknowledgement off lowers a
        // sticky ACKS down to the current SEVR — an alarm that has already
        // cleared must not keep a higher unacknowledged severity.
        if !new_ackt && self.common.acks > self.common.sevr {
            self.common.acks = self.common.sevr;
            self.notify_field_backed("ACKS", ack_mask, backing);
        }
        self.notify_record_alarm(backing);
    }

    /// C `dbAccess.c::putAcks` (`:1302-1315`) — the **only** runtime writer of
    /// ACKS. Reached from `dbPut` for a `DBR_PUT_ACKS` request type, ABOVE the
    /// `SPC_NOMOD` gate.
    ///
    /// The acknowledged severity is compared against the STORED unacknowledged
    /// severity `acks`, not the current `sevr`: an operator acknowledging at
    /// the severity that was latched into ACKS clears it even after `sevr` has
    /// since dropped. A too-low acknowledgement changes nothing and posts
    /// nothing; an acknowledgement of an already-clear ACKS still posts, which
    /// is C's literal `if (*psev >= precord->acks)` (0 >= 0 holds).
    pub fn put_acks(&mut self, value: u16, backing: LinkBacking<'_>) {
        let sev = AlarmSeverity::from_u16(value);
        if sev < self.common.acks {
            return;
        }
        use crate::server::recgbl::EventMask;
        self.common.acks = AlarmSeverity::NoAlarm;
        self.cleanup_subscribers();
        self.notify_field_backed("ACKS", EventMask::VALUE | EventMask::ALARM, backing);
        self.notify_record_alarm(backing);
    }

    /// Set a common field value from the `.db` loader, which in C is a
    /// different converter with a different out-of-menu bound
    /// (`dbStaticRun.c::dbPutStringNum`; see `MenuBound::DbLoad`). It is what
    /// lets `field(SSCN,"65535")` — the menuScan "use SCAN" sentinel, out of
    /// the menu's 0-9 range — load, while `caput REC.SSCN 65535` is refused at
    /// runtime exactly as C refuses it.
    pub fn put_common_field_db_load(
        &mut self,
        name: &str,
        value: EpicsValue,
    ) -> CaResult<CommonFieldPutResult> {
        self.put_common_field_bounded(name, value, MenuBound::DbLoad)
    }

    fn put_common_field_bounded(
        &mut self,
        name: &str,
        value: EpicsValue,
        bound: MenuBound,
    ) -> CaResult<CommonFieldPutResult> {
        let name = name.to_ascii_uppercase();
        self.record.validate_put(&name, &value)?;
        self.record.special(&name, false)?;
        // The db loader hands every common field to this path as a raw
        // `EpicsValue::String` (no per-field `FieldDesc` to parse against).
        // Coerce it to the field's canonical numeric/menu type up front so the
        // typed arms below apply a `field(PHAS, "1")` / `field(PRIO, "HIGH")`
        // directive instead of silently dropping it at IOC load. String-typed
        // and already-typed values pass through unchanged.
        let declared = declared_field_type_of(self.record.as_ref(), &name);
        let value = match coerce_common_field(&name, value, bound, declared)? {
            Converted::Stored(v) => v,
            // C's converter returned success without storing (`cvt_st_ul`'s
            // skipped store): the field keeps its old value, so no arm below
            // runs and no SCAN/PHAS transition happened.
            Converted::Unchanged => return Ok(CommonFieldPutResult::NoChange),
        };
        // C `dbPutString`/`dbPutField` route every link field's text through
        // `dbParseLink`, whose brace arm hands it to `dbJLinkParse`
        // (`dbStaticLib.c:2280-2286`); an unusable JSON link is
        // `S_dbLib_badField` and the field never takes the value. This is the
        // one funnel every common link field crosses — SDIS, TSEL, FLNK, DOL,
        // SIML, SIOL, INP, OUT — on both the db-load and the runtime put path,
        // so the rule holds without being restated per field.
        if let EpicsValue::String(ref s) = value {
            let text = s.as_str_lossy();
            if text.trim_start().starts_with('{')
                && crate::types::dbf_link_class(self.record.record_type(), &name).is_some()
            {
                super::check_json_link_text(&text)?;
            }
        }
        match name.as_str() {
            // `special(SPC_NOMOD)` — C `dbPutSpecial` refuses the put with
            // `S_db_noMod` (dbAccess.c:123-127). OLDSIMM is written only by the
            // simulation-mode owner (`rec_gbl_save_simm`).
            "OLDSIMM" => return Err(CaError::ReadOnlyField(name)),
            "SEVR" => {
                if let EpicsValue::Short(v) = value {
                    self.common.sevr = AlarmSeverity::from_u16(v as u16);
                }
            }
            "STAT" => {
                if let EpicsValue::Short(v) = value {
                    self.common.stat = v as u16;
                }
            }
            "NSEV" => {
                if let EpicsValue::Short(v) = value {
                    self.common.nsev = AlarmSeverity::from_u16(v as u16);
                }
            }
            "NSTA" => {
                if let EpicsValue::Short(v) = value {
                    self.common.nsta = v as u16;
                }
            }
            "AMSG" => {
                if let EpicsValue::String(s) = value {
                    self.common.amsg = s.as_str_lossy().into_owned();
                }
            }
            "NAMSG" => {
                if let EpicsValue::String(s) = value {
                    self.common.namsg = s.as_str_lossy().into_owned();
                }
            }
            // ACKS/ACKT carry NO acknowledgement semantics here. They are
            // `special(SPC_NOMOD)` in `dbCommon.dbd:150-159`, so no runtime put
            // reaches this arm — the gate refuses it. C's acknowledgement is
            // driven by the DBR *request type* (`DBR_PUT_ACKS`/`ACKT`), which
            // `dbPut` intercepts ABOVE the SPC_NOMOD gate and hands to
            // [`Self::put_acks`] / [`Self::put_ackt`]. What is left here is the
            // `dbLoadRecords` / `dbStaticLib` load path (`field(ACKT,"YES")`),
            // which stores the value verbatim — C `dbPutString` never crosses
            // `dbPut`.
            "ACKS" => {
                if let EpicsValue::Short(v) = value {
                    self.common.acks = AlarmSeverity::from_u16(v as u16);
                }
            }
            "ACKT" => match value {
                EpicsValue::Char(v) => self.common.ackt = v != 0,
                EpicsValue::Short(v) => self.common.ackt = v != 0,
                _ => return Ok(CommonFieldPutResult::NoChange),
            },
            "UDF" => {
                // Store the raw put byte (C keeps the epicsUInt8 verbatim); a
                // record that re-derives UDF on process overwrites it, one that
                // sources nothing this cycle keeps it (put-defect cluster #3).
                // The calc family reads UDF straight from `common` too
                // (`clears_udf() == false`), so the stored byte stands.
                if let EpicsValue::Char(v) = value {
                    self.common.udf = v;
                }
            }
            "UDFS" => {
                self.common.udfs = menu_ordinal_raw(&value);
            }
            // The `String` form never reaches these three menu arms:
            // `coerce_common_field` has already run it through the one
            // menu converter, which either produced an `Enum` index or failed
            // the put with `S_db_badChoice`.
            "SCAN" => {
                let new_scan = match &value {
                    EpicsValue::Short(v) => ScanType::from_u16(*v as u16),
                    EpicsValue::Enum(v) => ScanType::from_u16(*v),
                    _ => return Ok(CommonFieldPutResult::NoChange),
                };
                let result = self.set_scan(new_scan);
                if !matches!(result, CommonFieldPutResult::NoChange) {
                    self.record.on_put(&name);
                    self.record.special(&name, true)?;
                    return Ok(result);
                }
            }
            "SSCN" => {
                let new_sscn = match &value {
                    EpicsValue::Short(v) => SimModeScan::from_u16(*v as u16),
                    EpicsValue::Enum(v) => SimModeScan::from_u16(*v),
                    _ => return Ok(CommonFieldPutResult::NoChange),
                };
                self.common.sscn = new_sscn;
            }
            // `PINI` is `menu(menuPini)` — the six choices NO/YES/RUN/RUNNING/
            // PAUSE/PAUSED (`menuPini.dbd.pod:59-65`). Resolved exactly like
            // `SCAN`: a menu label or a bare index, never a truthiness test.
            // The pre-fix `bool` arm collapsed `RUN` (index 2) to `false`, so
            // `caput REC.PINI RUN` *disabled* PINI instead of selecting the
            // iocRun pass.
            "PINI" => {
                // Store the RAW ordinal (see [`CommonFields::pini`]): C's numeric
                // menu put keeps `(epicsEnum16)`, so an out-of-range `caput
                // REC.PINI 6` / `-1` round-trips and simply matches no lifecycle
                // pass in `doRecordPini`. A `String` label is already resolved to
                // `Enum` by `coerce_common_field` (menuPini via `putStringMenu`).
                self.common.pini = match &value {
                    EpicsValue::Short(v) => *v,
                    EpicsValue::Char(v) => *v as i16,
                    EpicsValue::Enum(v) => *v as i16,
                    _ => return Ok(CommonFieldPutResult::NoChange),
                };
            }
            "TPRO" => {
                if let EpicsValue::Char(v) = value {
                    self.common.tpro = v;
                }
            }
            "BKPT" => {
                if let EpicsValue::Char(v) = value {
                    self.common.bkpt = v;
                }
            }
            "FLNK" => {
                if let EpicsValue::String(s) = value {
                    self.common.flnk = s.as_str_lossy().into_owned();
                    self.parsed_flnk = parse_forward_link_v2(&self.common.flnk);
                }
            }
            "INP" => {
                // A record type whose C `.dbd` has no INP must refuse it, the
                // way C's dbd does ("field not found" at load, record inert) —
                // `histogram`'s input link is SVL, not INP
                // (histogramRecord.dbd.pod:212). Without this the port accepts a
                // `field(INP,...)` no C IOC can load.
                if !self.record.declares_inp_link() {
                    return Err(CaError::FieldNotFound("INP".to_string()));
                }
                if let EpicsValue::String(s) = value {
                    self.check_link_assignment("INP", &s.as_str_lossy(), bound)?;
                    self.common.inp = s.as_str_lossy().into_owned();
                    if !self.record_declares_field("INP") {
                        self.parsed_inp = parse_link_v2(&self.common.inp);
                    }
                }
            }
            "OUT" => {
                if let EpicsValue::String(s) = value {
                    let s = s.as_str_lossy();
                    self.check_link_assignment("OUT", &s, bound)?;
                    // C `dbParseLink` (dbStaticLib.c:2382-2386) discards a
                    // CP/CPP modifier on a DBF_OUTLINK and warns once, naming
                    // the holder record, its field and the target. The discard
                    // itself is owned by `parse_output_link_v2` below; only the
                    // diagnostic lives here, where the record name exists and
                    // the link text is being (re)loaded rather than re-parsed
                    // per process cycle.
                    if out_link_discards_cp(&s) {
                        tracing::warn!(
                            target: "epics_base_rs::record",
                            record = %self.name,
                            field = "OUT",
                            link = %s,
                            "Discarding CP/CPP modifier in CA output link"
                        );
                    }
                    self.common.out = s.into_owned();
                    // C `dbDbPutValue` (dbDbLink.c:386-389): an OUT
                    // link processes its target only on an explicit
                    // ` PP` token (or a `.PROC` destination). A bare
                    // OUT link is NPP — `parse_output_link_v2`
                    // downgrades the modifier-less `ProcessPassive`
                    // default that `parse_link_v2` would otherwise
                    // apply.
                    if !self.record_declares_field("OUT") {
                        self.parsed_out = parse_output_link_v2(&self.common.out);
                    }
                    // C `longoutRecord.c::special` (PR #6c573b4 part 2)
                    // and similar OOCH-style hooks need `after=true`
                    // to fire after the link has actually moved. The
                    // earlier `validate_put` + `special(name, false)`
                    // pair only covered the before-side.
                    self.record.special(&name, true)?;
                }
            }
            // Two shapes reach DTYP and both name a device support:
            //
            // * the `.db` loader hands over the NAME verbatim, and it may be a
            //   name registered at runtime by a downstream crate ("asynInt32")
            //   that no vendored `.dbd` declares — C would reject that at load,
            //   the port's registry accepts it (Tier 3);
            // * a `dbPut` arrives as the menu INDEX, because `DBF_DEVICE` is
            //   served as `DBR_ENUM` and `coerce_put_value` already resolved an
            //   incoming label through the device menu (C `putStringMenu`,
            //   which fails `S_db_badChoice` on a name the menu does not have).
            //
            // The index is meaningful against the MERGED device menu (static
            // `device()` declarations + runtime-contributed device support) —
            // the exact list `coerce_put_value` bounded it by. Resolving it
            // against the static-only menu here would drop every contributed
            // name (asyn's "asynInt32", scaler-rs's "Asyn Scaler") back to
            // NoChange, leaving DTYP unset after a valid put.
            "DTYP" => match value {
                EpicsValue::String(s) => self.common.dtyp = s.as_str_lossy().into_owned(),
                EpicsValue::Enum(i) => {
                    let merged = super::merged_device_menu(self.record.record_type());
                    match merged.get(i as usize) {
                        Some(name) => self.common.dtyp = (*name).to_string(),
                        None => return Ok(CommonFieldPutResult::NoChange),
                    }
                }
                _ => return Ok(CommonFieldPutResult::NoChange),
            },
            "TSE" => {
                if let EpicsValue::Short(v) = value {
                    self.common.tse = v;
                }
            }
            "TSEL" => {
                if let EpicsValue::String(s) = value {
                    self.common.tsel = s.as_str_lossy().into_owned();
                    self.parsed_tsel = parse_link_v2(&self.common.tsel);
                }
            }
            "UTAG" => {
                // C UTAG is DBF_UINT64 — accept any integer-shaped value and
                // store the unsigned 64-bit tag. The db loader feeds every
                // common field as EpicsValue::String, so parse field(UTAG, "N")
                // rather than dropping it silently at IOC load; a CA write to
                // this u64 field crosses as DBR_DOUBLE (CA has no uint64 wire
                // type), so accept Double too.
                match value {
                    EpicsValue::UInt64(v) => self.common.utag = v,
                    EpicsValue::Int64(v) => self.common.utag = v as u64,
                    EpicsValue::Long(v) => self.common.utag = v as u64,
                    EpicsValue::Short(v) => self.common.utag = v as u64,
                    EpicsValue::Enum(v) => self.common.utag = v as u64,
                    EpicsValue::Char(v) => self.common.utag = v as u64,
                    EpicsValue::Double(v) => self.common.utag = v as u64,
                    EpicsValue::String(s) => {
                        if let Ok(EpicsValue::UInt64(v)) =
                            EpicsValue::parse(DbFieldType::UInt64, s.as_str_lossy().trim())
                        {
                            self.common.utag = v;
                        }
                    }
                    _ => {}
                }
            }
            "ASG" => {
                if let EpicsValue::String(s) = value {
                    self.common.asg = s.as_str_lossy().into_owned();
                }
            }
            "ASL" => {
                // C dbCommon.ASL is `epicsUInt32` in the .dbd but
                // only ever 0 or 1; accept Char / Short / Long for
                // the common put paths and clamp to {0, 1}.
                // db_loader feeds every common field as
                // `EpicsValue::String`; also accept that so a
                // `.db` `field(ASL, "1")` directive isn't silently
                // ignored at IOC load.
                let n: i64 = match value {
                    EpicsValue::Char(v) => v as i64,
                    EpicsValue::Short(v) => v as i64,
                    EpicsValue::Long(v) => v as i64,
                    EpicsValue::Int64(v) => v,
                    EpicsValue::String(s) => s.as_str_lossy().trim().parse().unwrap_or(0),
                    _ => return Ok(CommonFieldPutResult::NoChange),
                };
                self.common.asl = if n != 0 { 1 } else { 0 };
            }
            "DESC" => {
                if let EpicsValue::String(s) = value {
                    // DBF_STRING data field — store the bytes verbatim so a
                    // non-UTF-8 DESC round-trips unchanged.
                    if self.common.desc != s {
                        self.common.desc = s;
                        // DESC feeds `display.description` (a metadata-cache
                        // source) but is not property-class — C never marks
                        // it prop(YES) (epics-base#785) — so refresh the
                        // cache here at the write owner without posting
                        // DBE_PROPERTY: the pvxs behavior (fresh on the next
                        // metadata build, no event).
                        self.invalidate_metadata_cache();
                    }
                }
            }
            "PHAS" => {
                if let EpicsValue::Short(v) = value {
                    let old_phas = self.common.phas;
                    self.common.phas = v;
                    // Only a record that IS in a scan list can be re-sorted
                    // within one; the same gate the index owner applies.
                    if old_phas != v && self.common.scan.scan_list().is_some() {
                        let scan = self.common.scan;
                        self.record.on_put(&name);
                        self.record.special(&name, true)?;
                        return Ok(CommonFieldPutResult::PhasChanged {
                            scan,
                            old_phas,
                            new_phas: v,
                        });
                    }
                }
            }
            "EVNT" => {
                // C `EVNT` is DBF_STRING (event name). Accept a
                // string directly; accept a numeric value too for
                // backward compatibility (numeric events / a calc
                // record driving EVNT) by formatting it as a string.
                match value {
                    EpicsValue::String(s) => self.common.evnt = s.as_str_lossy().into_owned(),
                    EpicsValue::Short(v) => self.common.evnt = v.to_string(),
                    EpicsValue::Long(v) => self.common.evnt = v.to_string(),
                    EpicsValue::Enum(v) => self.common.evnt = v.to_string(),
                    EpicsValue::Double(v) => {
                        // Match C `eventNameToHandle`: a double with
                        // an integer part is treated as that integer.
                        self.common.evnt = (v as i64).to_string();
                    }
                    _ => {}
                }
            }
            "PRIO" => {
                if let EpicsValue::Short(v) = value {
                    self.common.prio = v;
                }
            }
            "DISV" => {
                if let EpicsValue::Short(v) = value {
                    self.common.disv = v;
                }
            }
            "DISA" => {
                if let EpicsValue::Short(v) = value {
                    self.common.disa = v;
                }
            }
            "SDIS" => {
                if let EpicsValue::String(s) = value {
                    self.common.sdis = s.as_str_lossy().into_owned();
                    self.parsed_sdis = parse_link_v2(&self.common.sdis);
                }
            }
            "DISS" => {
                self.common.diss = menu_ordinal_raw(&value);
            }
            "HYST" => {
                if let Some(v) = value.to_f64() {
                    self.common.hyst = v;
                }
            }
            "LCNT" => {
                if let EpicsValue::Short(v) = value {
                    self.common.lcnt = v;
                }
            }
            "DISP" => {
                if let EpicsValue::Char(v) = value {
                    self.common.disp = v;
                }
            }
            "PUTF" => return Err(CaError::ReadOnlyField("PUTF".into())),
            "RPRO" => {
                if let EpicsValue::Char(v) = value {
                    self.common.rpro = v;
                }
            }
            "PACT" => return Err(CaError::ReadOnlyField("PACT".into())),
            // C `dbPut` stores the raw byte in `prec->proc` (retained across
            // processing — C never resets it); `coerce_common_field` has
            // already projected the put onto `DBF_UCHAR` (→ `Char`). The
            // `pp(TRUE)` reprocess is driven separately by the put-path
            // force-process intercept, so this arm ONLY records the byte.
            "PROC" => {
                if let EpicsValue::Char(v) = value {
                    self.common.proc_field = v;
                }
            }
            // Analog alarm limits. The DB-load String was already coerced to
            // the field's DECLARED `.dbd` type by `coerce_common_field` — the
            // one owner of "what type does this common field hold" — so every
            // writer (`.db` load, `caput`, a link) lands here with a numeric
            // value in the record's own alarm domain, `epicsInt64` included.
            "HIHI" => {
                if let (Some(v), Some(a)) = (
                    AlarmLimit::from_stored(&value),
                    self.common.analog_alarm.as_mut(),
                ) {
                    a.hihi = v;
                }
            }
            "HIGH" => {
                if let (Some(v), Some(a)) = (
                    AlarmLimit::from_stored(&value),
                    self.common.analog_alarm.as_mut(),
                ) {
                    a.high = v;
                }
            }
            "LOW" => {
                if let (Some(v), Some(a)) = (
                    AlarmLimit::from_stored(&value),
                    self.common.analog_alarm.as_mut(),
                ) {
                    a.low = v;
                }
            }
            "LOLO" => {
                if let (Some(v), Some(a)) = (
                    AlarmLimit::from_stored(&value),
                    self.common.analog_alarm.as_mut(),
                ) {
                    a.lolo = v;
                }
            }
            "HHSV" => {
                if let Some(a) = &mut self.common.analog_alarm {
                    a.hhsv = menu_ordinal_raw(&value);
                }
            }
            "HSV" => {
                if let Some(a) = &mut self.common.analog_alarm {
                    a.hsv = menu_ordinal_raw(&value);
                }
            }
            "LSV" => {
                if let Some(a) = &mut self.common.analog_alarm {
                    a.lsv = menu_ordinal_raw(&value);
                }
            }
            "LLSV" => {
                if let Some(a) = &mut self.common.analog_alarm {
                    a.llsv = menu_ordinal_raw(&value);
                }
            }
            // swait-specific: OUTN is the output link name for swait records.
            // Mirrors to common.out so the processing framework dispatches it.
            "OUTN" => {
                if self.record.record_type() != "swait" {
                    // No OUTN field on any other record type — the same
                    // `S_dbLib_fieldNotFound` the catch-all below reports.
                    return Err(self.unknown_field_error(name));
                }
                if let EpicsValue::String(s) = value {
                    self.common.out = s.as_str_lossy().into_owned();
                    // Bare OUT link is NPP — see the "OUT" arm.
                    self.parsed_out = parse_output_link_v2(&self.common.out);
                }
            }
            // C `dbNameToAddr` (dbAccess.c:660-676) resolves the field part
            // with `dbFindFieldPart`, then falls back to `dbGetAttributePart`.
            // A name that is neither a record field, nor a dbCommon field, nor
            // an attribute resolves to nothing (`S_dbLib_fieldNotFound`), so
            // `dbPutField` is never reached and the caller reports the error —
            // `dbpf` prints "PV '%s' not found" and returns -1 (dbTest.c:787-795).
            // Returning success here made a put to a misspelled field a silent
            // no-op.
            //
            // But a field the record's `.dbd` DECLARES and no arm above stored
            // is NOT unknown: C `dbPut` writes it into record memory even when
            // no record code reads it back (`caput dfanout.HOPR 10`). Land it in
            // the per-instance declared-override store — the write analog of
            // `declared_default` — so the put is accepted and a later read
            // reflects it. `put_declared_override` still returns
            // `unknown_field_error` for a name with no `dbFldDes`, so a
            // misspelled field is refused exactly as before.
            _ => return self.put_declared_override(&name, value, bound),
        }
        self.record.on_put(&name);
        // C `dbPut` (dbAccess.c:1399-1405) returns the after-put
        // `dbPutSpecial(paddr, 1)` status to the caller — the stored value
        // stays, but the monitor post and the process are skipped and the
        // client sees the failure. Never drop it.
        self.record.special(&name, true)?;
        Ok(CommonFieldPutResult::NoChange)
    }

    /// The error C reports for a write to a field name that
    /// [`Self::put_common_field`] does not own.
    ///
    /// Two C outcomes, split by whether the name resolves at all:
    ///
    /// - A record *attribute* (`NAME`, `RTYP`) resolves — `dbGetAttributePart`
    ///   succeeds — but the write is refused: `NAME` is `special(SPC_NOMOD)`
    ///   (dbCommon.dbd:13-17) so `dbPutSpecial` pass 0 returns `S_db_noMod`
    ///   (dbAccess.c:123-124), and an attribute address carries
    ///   `special == SPC_ATTRIBUTE`, which `dbPutField` rejects with the same
    ///   `S_db_noMod` (dbAccess.c:1252-1253).
    /// - Anything else does not resolve: `S_dbLib_fieldNotFound`.
    fn unknown_field_error(&self, name: String) -> CaError {
        if self.get_virtual_field(&name).is_some() {
            CaError::ReadOnlyField(name)
        } else {
            CaError::FieldNotFound(name)
        }
    }

    /// Store a put to a field the record's `.dbd` DECLARES but the record
    /// models no storage for — the WRITE owner of [`Self::declared_overrides`]
    /// and the write analog of [`Self::declared_default`].
    ///
    /// Reached only from [`Self::put_common_field_bounded`]'s catch-all, i.e.
    /// after both `Record::put_field` (returned `FieldNotFound`) and every
    /// `dbCommon` arm above have declined the field. Three gates, mirroring
    /// C `dbNameToAddr`/`dbPut`:
    ///
    /// * NO `dbFldDes` (`field_desc` is `None`) — the name is not a field of
    ///   this record type at all. C resolves nothing and `dbPutField` reports
    ///   `S_dbLib_fieldNotFound`; return [`Self::unknown_field_error`] (which
    ///   also renders `NAME`/`RTYP` as the read-only attributes they are).
    /// * `special(SPC_NOMOD)` — a declared field that is immutable
    ///   ([`Self::is_no_mod`]: the `.dbd` `read_only`/attribute bit or the
    ///   record's runtime `field_no_mod`). C refuses the put with `S_db_noMod`;
    ///   the runtime dispatch already gates this via `field_io::check_no_mod`,
    ///   but the db-load path does not, so enforce it here too — never store an
    ///   SPC_NOMOD field in the override map.
    /// * [`FieldDesc::runtime_typed`] — a field whose served type C's
    ///   `cvt_dbaddr` re-derives from record state (`waveform.VAL` from `FTVL`,
    ///   `aSub.A` from `FTA`). Such a field is record-owned by definition, so
    ///   its `put_field` should have taken the put; if it somehow reached here
    ///   the override store must not shadow it (`declared_default` skips it for
    ///   the same reason). Treat as not-found rather than store a value under
    ///   the wrong type.
    /// * PARTIALLY modeled — `Record::get_field` serves the field but no
    ///   `put_field` arm accepts it (`calcout.PVAL` → `self.pval`). The record
    ///   owns the read path, so the write belongs in its own `put_field`, not a
    ///   shadow cell; refuse here rather than store a value `resolve_field`
    ///   would never reach. See the inline note on the `get_field` guard.
    ///
    /// Otherwise coerce the incoming value to the field's C-declared DBF type
    /// through the one write-side value-coercion owner
    /// ([`coerce_put_value`](crate::server::record::coerce_put_value)) — so a
    /// `.db`/`caput` string parses with C's range rules (`caput REC.PREC 99999`
    /// into a `DBF_SHORT` is refused, not wrapped) and a menu label resolves
    /// against the field's own choices — and store it. Returns
    /// [`CommonFieldPutResult::NoChange`]: there is no scan/phas/alarm side
    /// effect for a metadata field with no record behaviour, and the caller's
    /// value-field monitor post reads the stored value back through
    /// [`Self::resolve_field`].
    fn put_declared_override(
        &mut self,
        name: &str,
        value: EpicsValue,
        bound: MenuBound,
    ) -> CaResult<CommonFieldPutResult> {
        let Some(desc) = self.field_desc(name) else {
            return Err(self.unknown_field_error(name.to_string()));
        };
        if desc.runtime_typed {
            return Err(self.unknown_field_error(name.to_string()));
        }
        if matches!(bound, MenuBound::DbPut) && self.is_no_mod(name) {
            // C `dbPutSpecial` pass 0 refuses SPC_NOMOD with `S_db_noMod`
            // (dbAccess.c:123-127) — and `dbPutSpecial` is reached only from
            // `dbPutField`/`dbPut`, the RUNTIME path. `dbLoadRecords` writes
            // through dbStatic's `dbPutString` (dbStaticLib.c:2570), which
            // consults `special` for `SPC_CALC` alone; SPC_NOMOD appears in
            // that layer only as a filter on `dbLexRoutines.c:1285`'s
            // misspelled-field guesser, never as a refusal of a field the
            // `.db` names outright. Refusing both paths dropped every
            // `field(<SPC_NOMOD>,…)` directive with a stderr line —
            // `mca`'s SIOL/SIML, `sub`'s LA..LU, `sel`'s LA..NLST,
            // `scalcout`'s PA..MLST, `asyn`'s AINP/NORD/ERRS and `swait`'s
            // VERS — so a simulated `mca` could not be given a SIOL at all.
            return Err(CaError::ReadOnlyField(name.to_string()));
        }
        // The override is the WRITABLE TWIN of `declared_default`, and
        // `declared_default` is `resolve_field`'s fallback ONLY when the record
        // itself serves nothing (`Record::get_field` is `None`). If the record
        // DOES serve this field (`get_field` is `Some`), it is not unmodeled —
        // it is PARTIALLY modeled: a getter into record memory (e.g.
        // `calcout.PVAL` → `self.pval`, which `process()` also writes) but no
        // matching `put_field` arm. Storing here would place the value in a
        // second cell that `resolve_field` never reaches (`get_field` shadows
        // the override) and that no `process()` keeps in step — a silent write
        // loss. Such a field's put belongs in the record's OWN `put_field`
        // (a per-record setter, a distinct change); refuse it here rather than
        // half-accept it, so `resolve_field` stays single-valued. A field the
        // record does not serve at all falls through to be stored.
        if self.record.get_field(name).is_some() {
            return Err(self.unknown_field_error(name.to_string()));
        }
        let target = desc.dbf_type;
        match crate::server::record::coerce_put_value(self.record.as_ref(), name, target, value)? {
            Converted::Stored(coerced) => {
                self.declared_overrides
                    .insert(name.to_ascii_uppercase(), coerced);
            }
            // Nothing stored: the override keeps whatever it held.
            Converted::Unchanged => {}
        }
        Ok(CommonFieldPutResult::NoChange)
    }

    /// Get virtual fields (NAME, RTYP).
    pub fn get_virtual_field(&self, name: &str) -> Option<EpicsValue> {
        match name {
            "NAME" => Some(EpicsValue::String(self.name.clone().into())),
            "RTYP" => Some(EpicsValue::String(
                self.record.record_type().to_string().into(),
            )),
            _ => None,
        }
    }

    /// Evaluate alarms based on record type and current value.
    /// Uses rec_gbl_set_sevr to accumulate into nsta/nsev.
    ///
    /// CALC_ALARM is NOT raised here. C raises it inside the record's own
    /// `process()` (`calcRecord.c:121-123`, `calcoutRecord.c:238-241`,
    /// `sCalcoutRecord.c:357-363`, `aCalcoutRecord.c:304-305`,
    /// `swaitRecord.c:409-410`), and in the port [`Record::check_alarms`] — which
    /// runs immediately before this — is that owner. It used to be raised here
    /// instead, keyed on a hardcoded `rtype` list plus a `CALC_ALARM` pseudo-field
    /// no DBD declares; swait is what that construction cost: it carried the flag
    /// but was not on the list, so a failed `calcPerform` alarmed nowhere.
    pub fn evaluate_alarms(&mut self) {
        use crate::server::recgbl;

        // Check UDF first — but only for record types whose C support carries
        // the `if (prec->udf) recGblSetSevr(..., UDF_ALARM, ...)` guard. C has
        // no central UDF alarm; see `Record::raises_udf_alarm`.
        if self.record.raises_udf_alarm() {
            recgbl::rec_gbl_check_udf(
                &mut self.common,
                self.record.udf_alarm_on_exact_one(),
                self.record.udf_alarm_severity(),
                self.record.udf_alarm_message(),
            );
        }

        // The analog-alarm SLOT is the enumeration — a record has the ladder iff
        // `new_boxed` gave it a config, which is the one place the C `.dbd`
        // survey lives. A second `match rtype` here was the same list written
        // twice, and the two could disagree: scalcout was in neither, so its ten
        // C alarm fields could not even be put.
        //
        // bi / bo / busy / mbbi / mbbo STATE+COS (and mbbo SOFT) alarm evaluation
        // lives in each record's `Record::check_alarms` hook (C `checkAlarms`);
        // those records carry no analog config, so they never reach here and
        // cannot double-raise.
        if let Some(ref alarm_cfg) = self.common.analog_alarm.clone() {
            // VAL goes down in the variant the record stores it in, not
            // flattened to `f64`: it is what picks the ladder's comparison
            // domain, and `Int64(v) as f64` had already rounded the value
            // before the first comparison ran.
            let val = match self.record.val() {
                Some(v @ (EpicsValue::Double(_) | EpicsValue::Long(_) | EpicsValue::Int64(_))) => v,
                _ => return,
            };
            self.evaluate_analog_alarm(val, alarm_cfg);
        }
    }

    fn evaluate_analog_alarm(&mut self, val: EpicsValue, cfg: &AnalogAlarmConfig) {
        use crate::server::recgbl::{self, alarm_status};

        // C `checkAlarms` returns immediately on a UDF cycle: it raises
        // `UDF_ALARM`/`UDFS` (already done by `rec_gbl_check_udf` in
        // `evaluate_alarms`), zeroes `AFVL` on the AFTC-capable records, and
        // returns BEFORE the range check — so `LALM` is left untouched and
        // `AFVL` is not filtered this cycle. The identical guard appears in
        // every record that shares this arm (`aiRecord.c:319-323`,
        // `aoRecord.c:383-386`, `longinRecord.c:274-278`,
        // `longoutRecord.c:317-320`, `int64inRecord.c:267-271`,
        // `int64outRecord.c:298-301`, `calcRecord.c:300-304`,
        // `calcoutRecord.c:563-566`). AFTC-capable records (ai/longin/
        // int64in/calc) carry `AFVL` and zero it (`prec->afvl = 0`); the
        // out records (ao/longout/int64out/calcout) have no `AFVL` and just
        // return. Running the range check here would drift `LALM` to `val`
        // (NaN on an undefined cycle) and filter `AFVL` — both observable.
        if self.common.udf != 0 {
            if matches!(
                self.record.record_type(),
                "calc" | "ai" | "longin" | "int64in"
            ) && self.record.get_field("AFVL").and_then(|v| v.to_f64()) != Some(0.0)
            {
                let _ = self.record.put_field("AFVL", EpicsValue::Double(0.0));
            }
            return;
        }

        // One rule for every ladder input: a record that DECLARES the field
        // owns it, because `Record::put_field` absorbs the client's put before
        // `put_common_field` ever runs, and only an undeclared field falls
        // through to `CommonFields`. MDEL/ADEL/MLST/ALST in
        // `check_monitor_deadbands` already read this way; HYST did not, so
        // `int64in`/`int64out`'s `pub hyst` swallowed every put while the
        // hysteresis compared against a permanent 0.0 — with `caget .HYST`
        // reading the value back, which is what made it silent.
        //
        // `common.hyst` stays an `f64` and stays exact: after the limits moved
        // to the declared type its only remaining readers are the
        // `DBF_DOUBLE` records and longin/longout, and every `epicsInt32` is
        // an `f64` exactly.
        let hyst_field = self.record.get_field("HYST");
        let lalm_field = self.record.get_field("LALM");

        // C-style per-level hysteresis: alarm fires if val passes the level,
        // OR if we were already at that alarm level (lalm == alev) and val
        // hasn't retreated past the hysteresis margin.
        //
        // `alarm_range` is the C-style integer level: 1=Lolo, 2=Low,
        // 3=Normal, 4=High, 5=Hihi. Required for the calc-record AFTC
        // filter (`calcRecord.c::checkAlarms:339-381`) which filters
        // on the range level (not on severity) and re-maps back.
        // C's `checkAlarms` enables each level with a NONZERO test on the raw
        // severity ordinal (`if (prec->hhsv && …)`) and passes that raw ordinal
        // to `recGblSetSevr`; `recGblResetAlarms` then clamps the resulting
        // *severity* to `INVALID_ALARM` while the *status* keeps the level. So an
        // out-of-range selector (`HHSV = 4`) still fires HIHI and lands
        // SEVR=INVALID/STAT=HIHI — reproduced by testing `!= 0` and mapping the
        // ordinal through [`AlarmSeverity::from_u16`] (which clamps `>= 3` to
        // `Invalid`).
        let sevs = [cfg.hhsv, cfg.llsv, cfg.hsv, cfg.lsv];
        let mut alarm_range = match &val {
            // The `DBF_LONG`/`DBF_INT64` records. `convert_to(Int64)` is the
            // workspace's one value-coercion owner, so a limit, a hysteresis
            // and a LALM all land here as the exact `epicsInt64` C compares.
            EpicsValue::Long(_) | EpicsValue::Int64(_) => {
                let int = |v: Option<EpicsValue>, dflt: i128| -> i128 {
                    match v.map(|v| v.convert_to(DbFieldType::Int64)) {
                        Some(EpicsValue::Int64(i)) => i as i128,
                        _ => dflt,
                    }
                };
                let v = int(Some(val.clone()), 0);
                super::alarm::analog_alarm_range(
                    v,
                    int(hyst_field, self.common.hyst as i128),
                    int(lalm_field, v),
                    [
                        cfg.hihi.as_i128(),
                        cfg.lolo.as_i128(),
                        cfg.high.as_i128(),
                        cfg.low.as_i128(),
                    ],
                    sevs,
                )
            }
            _ => {
                let v = val.to_f64().unwrap_or(0.0);
                super::alarm::analog_alarm_range(
                    v,
                    hyst_field
                        .and_then(|h| h.to_f64())
                        .unwrap_or(self.common.hyst),
                    lalm_field.and_then(|l| l.to_f64()).unwrap_or(v),
                    [
                        cfg.hihi.as_f64(),
                        cfg.lolo.as_f64(),
                        cfg.high.as_f64(),
                        cfg.low.as_f64(),
                    ],
                    sevs,
                )
            }
        };

        // C `range_stat[]` (`int64inRecord.c:250-253`) plus the severity and
        // `alev` each range selects. ONE table, because C reaches the same
        // mapping twice — once out of the ladder and once out of the AFTC
        // filter's `switch (alarmRange)` (`:326-346`).
        let resolve = |range: u16| -> (AlarmSeverity, u16, Option<AlarmLimit>) {
            match range {
                5 => (
                    AlarmSeverity::from_u16(cfg.hhsv as u16),
                    alarm_status::HIHI_ALARM,
                    Some(cfg.hihi),
                ),
                4 => (
                    AlarmSeverity::from_u16(cfg.hsv as u16),
                    alarm_status::HIGH_ALARM,
                    Some(cfg.high),
                ),
                2 => (
                    AlarmSeverity::from_u16(cfg.lsv as u16),
                    alarm_status::LOW_ALARM,
                    Some(cfg.low),
                ),
                1 => (
                    AlarmSeverity::from_u16(cfg.llsv as u16),
                    alarm_status::LOLO_ALARM,
                    Some(cfg.lolo),
                ),
                _ => (AlarmSeverity::NoAlarm, alarm_status::NO_ALARM, None),
            }
        };

        // C parity: the alarm-range AFTC low-pass filter
        // (`{ai,longin,int64in,calc}Record.c::checkAlarms`) smooths the
        // integer `alarmRange` and re-maps. Only records that carry the
        // AFTC/AFVL fields run it — `ao`/`longout`/`int64out`/`calcout`
        // have no AFTC field (confirmed via the respective `.dbd.pod`),
        // so they are excluded.
        let aftc_capable = matches!(
            self.record.record_type(),
            "calc" | "ai" | "longin" | "int64in"
        );
        if aftc_capable {
            let aftc = self
                .record
                .get_field("AFTC")
                .and_then(|v| v.to_f64())
                .unwrap_or(0.0);
            let afvl = self
                .record
                .get_field("AFVL")
                .and_then(|v| v.to_f64())
                .unwrap_or(0.0);
            if aftc > 0.0 {
                let now = crate::runtime::general_time::get_current();
                let (filtered_range, new_afvl) = crate::server::records::alarm_filter::aftc_filter(
                    alarm_range,
                    aftc,
                    afvl,
                    self.common.time,
                    now,
                );
                let _ = self.record.put_field("AFVL", EpicsValue::Double(new_afvl));
                // C re-maps through the SAME `switch (alarmRange)` the ladder
                // fell out of, so the filter changes only the range and
                // `resolve` below answers for both.
                alarm_range = filtered_range;
            } else {
                // aftc <= 0 disables the filter. C `checkAlarms`
                // (e.g. aiRecord.c:356,401) initialises the local
                // `afvl = 0` and unconditionally stores `prec->afvl =
                // afvl` at the end, so a disabled filter drives AFVL to
                // 0. Mirror that here so a stale accumulator from a prior
                // `aftc > 0` run cannot mis-seed the filter if AFTC is
                // re-enabled later.
                if afvl != 0.0 {
                    let _ = self.record.put_field("AFVL", EpicsValue::Double(0.0));
                }
            }
        }
        let (new_sevr, new_stat, alev) = resolve(alarm_range);

        if new_sevr != AlarmSeverity::NoAlarm {
            // C `aiRecord.c:405-406` — the latch is armed to the THRESHOLD, and
            // only when `recGblSetSevr` returns TRUE. A level that fires while a
            // higher-or-equal severity is already pending (an MS input link, a
            // SIMM alarm, a device INVALID) raises nothing, so C leaves LALM
            // where it was; arming it there would let the next cycle's
            // `lalm == alev && val >= alev - hyst` clause hold an alarm C has
            // already cleared.
            if recgbl::rec_gbl_set_sevr(&mut self.common, new_stat, new_sevr) {
                self.put_coerced("LALM", alev.map(|l| l.to_epics_value()).unwrap_or(val));
            }
        } else {
            // No alarm condition: reset LALM to current value. C `aiRecord.c:409`
            // does this unconditionally — only the alarm arm is gated.
            self.put_coerced("LALM", val);
        }
    }

    /// Invoke the registered subroutine (`sub`/`aSub` `SNAM`) if one is
    /// bound, before the record's `process()` body runs.
    ///
    /// C `subRecord.c::do_sub` / `aSubRecord.c::do_sub` call the named
    /// subroutine on EVERY `process()`. The function registry lives on the
    /// framework (`RecordInstance::subroutine`), not on the record, so the
    /// record's own `process()` is a no-op for these two types and the
    /// framework must drive the call. This is the SINGLE owner of that call
    /// for every dispatch path: the main engine
    /// (`process_record_with_links_inner`, the SCAN / event / CA-put-to-PP /
    /// FLNK path) and the by-name `process_local` (`db.process_record`,
    /// QSRV group / foreign-call path) both route through here, so a
    /// `sub`/`aSub` runs identically regardless of how it is processed.
    /// Previously only `process_local` invoked the subroutine, so on the
    /// main engine path `VAL`/`VALA..VALU`/`OUTA..OUTU` never updated.
    /// The cycle's status is delivered to the record on EVERY exit path — see
    /// [`Record::set_subroutine_status`], which aSub's OUT-link gate reads. The
    /// delivery is factored out of the body below so a future early return
    /// cannot skip it: the body returns the status, this wrapper publishes it.
    pub(crate) fn run_registered_subroutine(&mut self) -> CaResult<()> {
        let outcome = self.run_subroutine_body();
        // A subroutine that errored out has no C counterpart (a C subroutine
        // returns a `long`); it is a failed cycle, so it takes the non-zero
        // arm — no outputs.
        let status = *outcome.as_ref().unwrap_or(&SUBROUTINE_STATUS_ERROR);
        self.record.set_subroutine_status(status);
        outcome.map(|_| ())
    }

    /// Returns C `process`'s `status` for this cycle: 0 only when `do_sub` ran
    /// and returned 0.
    ///
    /// This is C `process`'s
    /// `if (!status) { status = do_sub(prec); prec->val = status; }`
    /// (aSubRecord.c:216-224, subRecord.c:142-147). The VAL publish is HERE
    /// rather than inside [`Self::do_sub`] precisely because C puts it here:
    /// every `do_sub` exit — empty SNAM, unregistered SNAM, the subroutine's
    /// own return — publishes its status as aSub's VAL from this one site, and
    /// only the pre-`do_sub` skip (a failed `fetch_values`) leaves VAL alone.
    fn run_subroutine_body(&mut self) -> CaResult<i64> {
        // aSub `LFLG=READ`: a `SUBL` re-resolution that found a bad/unregistered
        // name (C `fetch_values` -> `S_db_BadSub`) or failed to read the link
        // signals "skip do_sub this cycle" — C `process` runs `do_sub` only on
        // `!status`. The framework's failed input-link fetch arms the same flag.
        // One-shot: taken (cleared) whether or not a subroutine is set, so it
        // never leaks into the next cycle. The single consumer of the flag,
        // shared by every process path.
        if std::mem::take(&mut self.suppress_subroutine_run) {
            return Ok(SUBROUTINE_STATUS_SKIPPED);
        }

        // Every record type reaches this call on the process path, but only
        // `sub` and `aSub` have a `do_sub` in their rset at all. For all the
        // others "no subroutine is bound" is their permanent normal state, not
        // an unresolved SNAM, so they must not take `do_sub`'s bad-sub exit.
        let Some(kind) = SubroutineKind::of(self.record.record_type()) else {
            return Ok(SUBROUTINE_STATUS_SKIPPED);
        };

        let status = self.do_sub(kind)?;

        // aSub publishes the status as VAL (C `aSubRecord.c:224`
        // `prec->val = status`). The subroutine's computed outputs live in
        // VALA..VALU, so VAL is the return code and overwrites whatever the
        // closure may have written to VAL. `sub` does NOT do this — its VAL
        // is the value the subroutine computed. aSub VAL is DBF_LONG
        // (epicsInt32); the status is a C `long` truncated into it.
        if kind == SubroutineKind::ASub {
            let _ = self
                .record
                .put_field("VAL", EpicsValue::Long(status as i32));
        }
        Ok(status)
    }

    /// C `do_sub` — `aSubRecord.c:454-473` and `subRecord.c:420-437`, which
    /// differ in exactly two places and agree everywhere else:
    ///
    /// * aSub short-circuits an EMPTY SNAM to `return 0` BEFORE the null-pointer
    ///   check (`if (prec->snam[0] == 0) return 0;`), so a bare
    ///   `record(aSub,"X"){}` is a no-op that completes with status 0, not a
    ///   bad-sub. `sub` has no such branch — it cannot reach here with an empty
    ///   SNAM because `init_record` parks PACT
    ///   (`Record::init_record_parks_pact`, subRecord.c:119-123).
    /// * an unresolved subroutine raises `BAD_SUB_ALARM` at `INVALID_ALARM` in
    ///   both, but aSub returns `S_db_BadSub` (which `run_subroutine_body`
    ///   publishes as VAL and aSub's OUT gate reads as "push nothing") while
    ///   `sub` returns 0.
    ///
    /// The raise is per-cycle, not a one-shot init diagnostic: C `iocInit`
    /// discards `init_record`'s status (iocInit.c:569-570), so the record loads
    /// and scans and every process cycle re-raises BAD_SUB/INVALID.
    fn do_sub(&mut self, kind: SubroutineKind) -> CaResult<i64> {
        use crate::server::recgbl::{self, alarm_status};

        // Clone the Arc so the borrow on `self.subroutine` is released
        // before we mutate `self.record` / `self.common` below.
        let Some(sub_fn) = self.subroutine.clone() else {
            let snam_empty = matches!(
                self.record.get_field("SNAM"),
                Some(EpicsValue::String(s)) if s.is_empty()
            );
            if kind == SubroutineKind::ASub && snam_empty {
                return Ok(0);
            }
            recgbl::rec_gbl_set_sevr(
                &mut self.common,
                alarm_status::BAD_SUB_ALARM,
                AlarmSeverity::Invalid,
            );
            return Ok(match kind {
                SubroutineKind::ASub => S_DB_BAD_SUB,
                SubroutineKind::Sub => 0,
            });
        };
        // C `do_sub` returns the subroutine's `long` status.
        let status = sub_fn(&mut *self.record)?;

        // A negative status raises SOFT_ALARM at the record's BRSV severity
        // (C `do_sub`: `if (status < 0) recGblSetSevr(SOFT_ALARM,
        // prec->brsv)`). It accumulates into nsta/nsev for this cycle's
        // recGblResetAlarms commit and runs before checkAlarms, so a higher
        // analog severity (e.g. the shared analog-alarm owner) still wins via
        // the raise-only rule. BRSV defaults to NO_ALARM, under which
        // recGblSetSevr is a no-op.
        if status < 0 {
            let brsv = self
                .record
                .get_field("BRSV")
                .and_then(|v| v.to_f64())
                .map(|f| AlarmSeverity::from_u16(f as u16))
                .unwrap_or(AlarmSeverity::NoAlarm);
            recgbl::rec_gbl_set_sevr(&mut self.common, alarm_status::SOFT_ALARM, brsv);
        } else {
            // C `do_sub`'s `else` arm — the ONE place either flavour writes UDF,
            // reached only where the subroutine actually ran and returned `>= 0`.
            // aSub takes `prec->udf = FALSE` (`aSubRecord.c:470`); `sub` takes
            // `prec->udf = isnan(prec->val)` (`subRecord.c:434`).
            //
            // `sub`'s derive is the same expression the framework's per-cycle
            // blanket computes, made HERE instead of there so that the cycles
            // which run no subroutine — the unresolved-SNAM `BAD_SUB_ALARM`
            // return above, a failed `fetch_values` (`suppress_subroutine_run`),
            // a negative status — leave UDF at its previous value, exactly as C
            // does. Both flavours therefore opt out of the blanket
            // (`Record::clears_udf` == false).
            self.common.udf = match kind {
                SubroutineKind::ASub => 0,
                SubroutineKind::Sub => self.record.value_is_undefined() as u8,
            };
        }
        Ok(status)
    }

    /// The single owner of a process cycle's SUBSCRIBER posts — C `monitor()`'s
    /// "post every subscribed field this cycle touched" loop.
    ///
    /// Every processing path (`process_record_with_links_inner`, the deferred
    /// async-completion path, the simulation path, and [`Self::process_local`])
    /// calls this; none of them may reimplement the rules, because a rule that
    /// holds on one path and not another is a monitor that fires on a scan cycle
    /// but not on an async completion. The per-field mask resolvers
    /// ([`AuxPostMask`], [`crate::server::record::value_gate`]) were already
    /// single-owned for the same reason — this is the loop around them.
    ///
    /// It also UPDATES `last_posted` for everything it emits, and it TAKES the
    /// record's per-cycle post mask ([`Record::take_cycle_posted_fields`]), so
    /// it must run exactly once per cycle.
    ///
    /// The rules, in order:
    ///
    /// * The deadband field (default VAL), the
    ///   [`recgbl::RECGBL_POSTED_ALARM_FIELDS`](crate::server::recgbl::RECGBL_POSTED_ALARM_FIELDS)
    ///   (SEVR/STAT/AMSG/ACKS) and UDF are emitted by the caller with their own
    ///   C masks and are skipped here.
    /// * [`Record::event_posted_fields`] post from their own event path
    ///   (waveform HASH) — never from change detection.
    /// * [`Record::process_posted_fields`], when declared, is the closed set of
    ///   fields a process cycle may post at all.
    /// * A secondary value field ([`Record::fields_posted_with_value_mask`])
    ///   carries VAL's monitor mask, gated per its [`ValuePostGate`](super::ValuePostGate).
    /// * A CHANGED field carries [`AuxPostMask::mask_for`] — unless it is a
    ///   [`Record::fields_posted_only_when_marked`] field, which C never
    ///   change-detects (aCalcout AA..LL) and which therefore posts from its
    ///   mark alone.
    /// * An UNCHANGED field posts only if the record marked it this cycle:
    ///   statically ([`Record::force_posted_fields`]), per-cycle
    ///   ([`Record::take_cycle_posted_fields`]), on the alarm transition
    ///   ([`Record::alarm_cycle_monitored_fields`]), or in the DBE_LOG sweep
    ///   ([`Record::log_swept_fields`]).
    pub(crate) fn collect_subscriber_posts(
        &mut self,
        deadband_field: &str,
        deadband_mask: EventMask,
        alarm_bits: EventMask,
        aux_post: AuxPostMask,
        include_val: bool,
    ) -> Vec<(String, EpicsValue, EventMask)> {
        use crate::server::record::{CyclePostMask, ValuePostGate, value_gate};

        // C's default for a change-detected auxiliary post:
        // `monitor_mask | DBE_VALUE | DBE_LOG` (calcRecord.c:420, subRecord.c:400;
        // motor `DBE_VAL_LOG` for marked fields, motorRecord.cc:3522-3645).
        let aux_mask = alarm_bits | EventMask::VALUE | EventMask::LOG;
        let alarm_fanout: &[&str] = if alarm_bits.is_empty() {
            &[]
        } else {
            self.record.alarm_cycle_monitored_fields()
        };
        let force_fields = self.record.force_posted_fields();
        // TAKE — this also clears the state it answers from (C's
        // `pcalc->newm = 0`), which is why this loop may run only once per cycle.
        let mut cycle_posted = self.record.take_cycle_posted_fields();
        // The record-lifetime sibling: C's `firstCalcPosted == 0` term, which
        // iocInit's per-cycle drain must not be able to eat. Merged here so
        // both reach the same branch with the same mask mapping.
        cycle_posted.extend(self.record.take_first_monitor_cycle());
        let log_swept = self.record.log_swept_fields();
        // C change-detects nothing about these fields; only the record's own
        // per-cycle mark may post them (aCalcout AA..LL — no PAA..PLL previous
        // copy exists to compare against).
        let marked_only = self.record.fields_posted_only_when_marked();
        let value_masked = self.record.fields_posted_with_value_mask();
        // C `if (prec->omod) monitor_mask |= (DBE_VALUE|DBE_LOG)` — the guard
        // `OnChangeForced` fields sit behind, which the record may open on a
        // cycle where VAL's own mask is shut. TAKEn, like `cycle_posted`, so
        // this loop may run only once per cycle.
        let secondary_guard = deadband_mask | self.record.take_secondary_value_mask();
        let event_posted = self.record.event_posted_fields();
        let process_posted = self.record.process_posted_fields();

        let mut sub_updates: Vec<(String, EpicsValue, EventMask)> = Vec::new();
        // C aoRecord.c:536-549: the secondary block runs once per cycle, from
        // inside `if (monitor_mask)`, and each field's own `oraw != rval` test
        // is welded to the `oraw = rval` that follows its `db_post_events`.
        // Decided HERE and not in the subscriber walk below, for two reasons
        // the walk cannot satisfy: C's guard is the record's own old copy, not
        // this loop's `last_posted` change detection, and C posts whether or
        // not anyone is subscribed — so the bookkeeping must not depend on who
        // is watching.
        let forced_posts: Vec<(String, EpicsValue, EventMask)> = if secondary_guard.is_empty() {
            Vec::new()
        } else {
            let forced_mask = secondary_guard | EventMask::VALUE | EventMask::LOG;
            let forced: Vec<&'static str> = value_masked
                .iter()
                .filter(|(_, gate)| *gate == ValuePostGate::OnChangeForced)
                .map(|(name, _)| *name)
                .collect();
            let mut out = Vec::new();
            for name in forced {
                if !self.record.take_secondary_value_change(name) {
                    continue;
                }
                if let Some(val) = self.resolve_field(name) {
                    out.push((name.to_string(), val, forced_mask));
                }
            }
            out
        };
        for (field, subs) in &self.subscribers {
            if subs.is_empty()
                || field == deadband_field
                // SEVR/STAT/AMSG/ACKS are posted by `recGblResetAlarms` itself,
                // each with its own C mask (recGbl.c:202-222) — the caller emits
                // them from `alarm_field_posts`. A second, change-detected copy
                // here would double-post with a mask C never uses for them
                // (`alarm_bits | DBE_VALUE | DBE_LOG` instead of C's DBE_VALUE
                // on ACKS). UDF is excluded for the opposite reason: NO C
                // `monitor()` posts it at all, so a processing cycle that
                // redefines VAL must emit no `.UDF` event (a caput to `.UDF`
                // still posts, through the generic put path).
                || crate::server::recgbl::RECGBL_POSTED_ALARM_FIELDS.contains(&field.as_str())
                || field == "UDF"
                || event_posted.contains(&field.as_str())
                || !process_posted.is_none_or(|allowed| allowed.contains(&field.as_str()))
            {
                continue;
            }
            let Some(val) = self.resolve_field(field) else {
                continue;
            };
            let changed = match self.posted_value(field) {
                Some(prev) => prev != &val,
                None => true,
            };
            if let Some(gate) = value_gate(value_masked, field) {
                // C posts this secondary value field with VAL's own monitor_mask,
                // from inside the guard that decides whether VAL posts at all —
                // never a forced DBE_VALUE|DBE_LOG. `ValuePostGate` says whether C
                // also re-tests the field's own value inside that guard (ai RVAL,
                // aiRecord.c:462) or posts it whenever the guard fires (timestamp
                // RVAL, timestampRecord.c:160).
                let post = match gate {
                    ValuePostGate::OnChange => changed && !deadband_mask.is_empty(),
                    ValuePostGate::WithValue => include_val,
                    // Decided once per cycle in `forced_posts` above, against
                    // the record's own old copy — never here, where the answer
                    // would depend on this loop's `last_posted` cache and on
                    // the field having a subscriber.
                    ValuePostGate::OnChangeForced => false,
                };
                if post {
                    sub_updates.push((field.clone(), val.clone(), deadband_mask));
                }
            } else if changed && !marked_only.contains(&field.as_str()) {
                sub_updates.push((
                    field.clone(),
                    val.clone(),
                    aux_post.mask_for(field, alarm_bits, deadband_mask),
                ));
            } else if force_fields.contains(&field.as_str()) {
                // C `monitor()` posts a statically re-marked field with
                // `monitor_mask | DBE_VAL_LOG` even when unchanged.
                sub_updates.push((field.clone(), val.clone(), aux_mask));
            } else if cycle_posted.iter().any(|(name, _)| *name == field) {
                // One event per MARK, each with the mask of the C call site that
                // made it (`CyclePostMask`) — a field marked twice (aCalcout's
                // AMASK `afterCalc` post AND its NEWM `monitor()` post) is posted
                // twice, exactly as C posts it from both loops.
                for (_, cycle_mask) in cycle_posted.iter().filter(|(name, _)| *name == field) {
                    let mask = match cycle_mask {
                        CyclePostMask::Value => EventMask::VALUE,
                        CyclePostMask::ValueLog => EventMask::VALUE | EventMask::LOG,
                        CyclePostMask::MonitorValueLog => aux_mask,
                    };
                    sub_updates.push((field.clone(), val.clone(), mask));
                }
            } else if alarm_fanout.contains(&field.as_str()) {
                // C motor `monitor()` (motorRecord.cc:3456-3646) posts every listed
                // field once `monitor_mask != 0`, so a DBE_ALARM-only subscriber
                // observes the alarm moment on any of them.
                sub_updates.push((field.clone(), val.clone(), alarm_bits));
            }
            // C `scalerRecord.c::monitor():757-773` posts EVERY S1..Snch with a
            // literal DBE_LOG on every cycle it runs (it runs when `ss == IDLE`,
            // scalerRecord.c:510). That sweep is INDEPENDENT of the change post,
            // not an alternative to it: on the count-completion cycle `ss` is
            // IDLE and `updateCounts()` has ALREADY posted each changed Sn with
            // DBE_VALUE (:582), so C emits two events for that field in that one
            // cycle — DBE_VALUE, then DBE_LOG. Making this an `else if` on
            // `changed` dropped the DBE_LOG half exactly when it matters: a
            // DBE_LOG-only archiver would never receive the final counts.
            //
            // The sweep carries the ALARM-transition bits too. DEVIATION from C,
            // deliberate — CBUG-B19. C's `monitor()` opens with
            // `monitor_mask = recGblResetAlarms(pscal); monitor_mask |=
            // (DBE_VALUE|DBE_LOG);` and then posts with a LITERAL `DBE_LOG`
            // (scalerRecord.c:764-771) — `monitor_mask` is assigned, OR-ed, and
            // never read. Those two lines are dead, and their only plausible use
            // was as the third `db_post_events` argument.
            // `recGblResetAlarms` returns the alarm-transition mask that every
            // other record ORs into its value posts, so discarding it drops the
            // alarm bit: a client subscribed to `Sn` with DBE_ALARM receives
            // NOTHING on an alarm-severity transition of the record.
            //
            // The DBE_VALUE half of C's dead `|=` is deliberately NOT
            // resurrected: this sweep is unconditional, so adding VALUE would
            // fire a value event at every VALUE subscriber on every idle scan,
            // changed or not — that would be a new defect, not a fix. The value
            // path is separately served by the change post (C's `updateCounts()`
            // DBE_VALUE at `:582`).
            if log_swept.contains(&field.as_str()) {
                sub_updates.push((field.clone(), val, EventMask::LOG | alarm_bits));
            }
        }
        // A guarded secondary post reaches the snapshot only if the field has
        // a subscriber, exactly as every other branch of the walk above; C's
        // `db_post_events` with no subscriber delivers nothing either. The
        // record's old copy has already advanced regardless — that is the
        // half that must not depend on who is watching.
        for (field, val, mask) in forced_posts {
            if self.subscribers.get(&field).is_some_and(|s| !s.is_empty()) {
                sub_updates.push((field, val, mask));
            }
        }
        for (field, val, _) in &sub_updates {
            self.record_value_post(field, val.clone());
        }
        sub_updates
    }

    /// Basic process: process record, evaluate alarms, timestamp, build snapshot.
    /// This does NOT handle links — see process_with_context in database.rs.
    ///
    /// Returns the value/log snapshot plus a list of alarm-field posts
    /// (`SEVR`/`STAT`/`AMSG`/`ACKS`) with their individual C event masks.
    /// `SEVR` is posted `DBE_VALUE` only; `STAT`/`AMSG` carry `DBE_ALARM`
    /// (sevr/amsg change) and/or `DBE_VALUE` (stat change). The caller
    /// must fire these via `notify_field` so a `DBE_VALUE`-only `.SEVR`
    /// subscriber is not missed on an alarm-only change and a
    /// `DBE_ALARM`-only subscriber is not wrongly notified — C parity
    /// with `recGblResetAlarms` (recGbl.c:202-222), matching the
    /// `processing.rs` link path.
    pub fn process_local(
        &mut self,
    ) -> CaResult<(
        ProcessSnapshot,
        Vec<(&'static str, crate::server::recgbl::EventMask)>,
    )> {
        use crate::server::recgbl::{self, EventMask};
        const LCNT_ALARM_THRESHOLD: i16 = 10;

        if self.pact.swap(true, std::sync::atomic::Ordering::AcqRel) {
            // C `dbProcess` PACT-active guard (dbAccess.c:544-557):
            //
            //   if ((precord->stat == SCAN_ALARM) ||
            //       (precord->lcnt++ < MAX_LOCK) ||
            //       (precord->sevr >= INVALID_ALARM)) goto all_done;
            //   recGblSetSevrMsg(precord, SCAN_ALARM, INVALID_ALARM,
            //                    "Async in progress");
            //
            // The alarm fires EXACTLY ONCE — on the attempt whose
            // pre-increment lcnt equals MAX_LOCK — and is then blocked
            // by the stat == SCAN_ALARM / sevr >= INVALID bails, the
            // same shape as the link path
            // (`process_record_with_links_inner`). The pre-fix guard
            // here used post-increment `lcnt >= threshold` with no
            // already-raised bail, so every reentrant attempt past the
            // threshold re-posted the unchanged SEVR/STAT/VAL (and the
            // first fire came one attempt early); it also wrote
            // sevr/stat directly, skipping `recGblSetSevrMsg` +
            // `recGblResetAlarms` — losing the "Async in progress"
            // AMSG and the acks bookkeeping the reset performs.
            let already_scan_alarm = self.common.stat == recgbl::alarm_status::SCAN_ALARM;
            let already_invalid = self.common.sevr >= AlarmSeverity::Invalid;
            let lcnt_before = self.common.lcnt;
            self.common.lcnt = lcnt_before.saturating_add(1);
            if already_scan_alarm || lcnt_before < LCNT_ALARM_THRESHOLD || already_invalid {
                return Ok((
                    ProcessSnapshot {
                        changed_fields: Vec::new(),
                    },
                    Vec::new(),
                ));
            }
            recgbl::rec_gbl_set_sevr_msg(
                &mut self.common,
                recgbl::alarm_status::SCAN_ALARM,
                AlarmSeverity::Invalid,
                "Async in progress",
            );
            let _ = recgbl::rec_gbl_reset_alarms(&mut self.common);
            // Per-field C masks (recGbl.c:202-222): this guard only
            // runs on a fresh SCAN_ALARM/INVALID raise, so sevr AND
            // stat both moved — SEVR posts DBE_VALUE, STAT/AMSG post
            // the shared `stat_mask` = DBE_ALARM|DBE_VALUE, VAL posts
            // DBE_VALUE|DBE_LOG plus `val_mask` = DBE_ALARM.
            let stat_mask = EventMask::ALARM | EventMask::VALUE;
            let mut changed_fields = Vec::new();
            if let Some(val) = self.record.val() {
                changed_fields.push((
                    "VAL".to_string(),
                    val,
                    EventMask::VALUE | EventMask::LOG | EventMask::ALARM,
                ));
            }
            changed_fields.push((
                "SEVR".to_string(),
                EpicsValue::Short(self.common.sevr as i16),
                EventMask::VALUE,
            ));
            changed_fields.push((
                "STAT".to_string(),
                EpicsValue::Short(self.common.stat as i16),
                stat_mask,
            ));
            // AMSG carries "Async in progress" alongside the STAT
            // transition (C recGbl.c posts STAT and AMSG together
            // when any alarm field moved).
            changed_fields.push((
                "AMSG".to_string(),
                EpicsValue::String(self.common.amsg.clone().into()),
                stat_mask,
            ));
            return Ok((ProcessSnapshot { changed_fields }, Vec::new()));
        }
        self.common.lcnt = 0;
        // RAII guard that resets `self.pact` to false on drop — both for the
        // normal exit path and for any `?` early return. The guard holds a raw
        // pointer rather than a reference because we still need `self` mutably
        // while the guard is alive (the record body below mutates other `self`
        // fields).
        //
        // This is the one PACT release that does not go through `leave_pact`,
        // and it provably owes no restart: `process_local` holds `&mut self` for
        // the whole PACT window, and a put-notify is queued only through
        // `queue_notify_put`, which needs that same `&mut`. So nothing can join
        // the restart list inside the window, and the `swap(true)` above proved
        // the record was idle on entry.
        debug_assert!(
            self.notify_restart_list.is_empty(),
            "a queued put-notify implies the record was owned, which the swap \
             above proved it was not"
        );
        struct ProcessGuard(*const AtomicBool);
        // SAFETY: AtomicBool is Sync; raw pointers don't auto-derive
        // Send. We hand-roll Send because the ptr targets a field of
        // `self`, which the caller already proves can be borrowed
        // through this code path. The pointer is only ever read for an
        // atomic store, never written, dereferenced for raw access, or
        // escaped from this scope.
        unsafe impl Send for ProcessGuard {}
        impl Drop for ProcessGuard {
            fn drop(&mut self) {
                // SAFETY: `self.0` was constructed from
                // `&self.pact as *const AtomicBool` below, where
                // `self` is the live RecordInstance whose lifetime
                // strictly outlives `_guard`. RecordInstance is
                // !Unpin-equivalent in practice (we never move it
                // while held in the database's `Arc<RwLock<_>>`), so
                // the pointer remains valid until Drop runs.
                unsafe { &*self.0 }.store(false, std::sync::atomic::Ordering::Release);
            }
        }
        let _guard = ProcessGuard(&self.pact as *const AtomicBool);

        // Call subroutine if registered (for sub/aSub records). Single owner
        // shared with the main engine path — see `run_registered_subroutine`.
        self.run_registered_subroutine()?;
        // Soft-Channel input records must skip the RVAL->VAL convert
        // (C `devAiSoft.c` `read_ai` returns 2 = "don't convert" for
        // every Soft-Channel input record, incl. one with a constant /
        // unset INP). Without this, `process_local` on a soft input
        // with a preset VAL — e.g. NaN — would run `convert()` and
        // clobber it, after which the UDF check below would see a
        // defined value and wrongly clear UDF. The
        // `processing.rs` link path already does this; `process_local`
        // is the separate foreign-call path (`db.process_record`) and
        // needs the same skip. `SoftDtyp::Raw` is excluded below and still
        // runs convert.
        //
        // Gated on `soft_channel_skips_convert()` — identical to the
        // `processing.rs` link path — so this only suppresses the
        // `RVAL → VAL` convert step. `set_device_did_compute` is an
        // overloaded hook: `ai/bi/mbbi/mbbi_direct` read it as
        // "skip convert" (override true), but `epid` reads it as
        // "skip the whole built-in PID compute" (keeps default false).
        // Without this gate, a Soft-Channel `epid` driven through
        // `process_local` (`db.process_record`, e.g. QSRV group proc
        // members) would skip `do_pid()` entirely — the regression
        // d1032fe5 fixed on the `processing.rs` path only.
        {
            // The same "does the input dset return 2" question the
            // `processing.rs` link path asks — Plain and Async, not Raw.
            let is_soft = matches!(
                crate::server::device_support::classify_soft(&self.common.dtyp),
                Some(
                    crate::server::device_support::SoftDtyp::Plain
                        | crate::server::device_support::SoftDtyp::Async
                )
            );
            let is_output = self.record.can_device_write();
            if is_soft && !is_output && self.record.soft_channel_skips_convert() {
                self.record.set_device_did_compute(true);
            }
        }
        // Push framework-owned common state (UDF/PHAS/TSE/TSEL) so the
        // record's process() can see it — same as the processing.rs link
        // path. `process_local` is the foreign-call path
        // (`db.process_record`); without this a record driven through it
        // (e.g. QSRV group-process members) would not see UDF/TSE.
        {
            let ctx = self.common.process_context();
            self.record.set_process_context(&ctx);
        }
        let outcome = self.record.process()?;
        let process_result = outcome.result;
        // Note: process_local() does not execute ProcessActions — those are
        // handled by the full process_record_with_links() path in processing.rs.
        //
        // It must still apply `post_write_fields`. There are no link writes
        // here for them to be ordered against, so the ordering rule is
        // satisfied trivially; what is NOT optional is applying them at all —
        // a record that hands its completion-flag clear to the framework
        // (sseq's `busy`, scaler's `cnt`) would otherwise stay busy forever on
        // this path. Same store-then-`DBE_VALUE`-post as
        // `PvDatabase::publish_post_write_fields`.
        for (field, value) in outcome.post_write_fields {
            if self.record.put_field_internal(&field, value).is_ok() {
                self.notify_field_written(&field);
                self.notify_field(&field, crate::server::recgbl::EventMask::VALUE);
            }
        }

        // If the record reports it modified a metadata-class field during
        // process(), invalidate the metadata cache so the next snapshot
        // rebuilds from the new values. Default impl returns false, so
        // most records pay zero cost here.
        if self.record.took_metadata_change() {
            self.invalidate_metadata_cache();
            // mirror C db_post_events(precord, NULL, DBE_PROPERTY) after record processing.
            // `none()` and not a parameter, alone among the `DBE_PROPERTY`
            // sweeps: this function is the link-LESS process path by its own
            // contract above, and every production process cycle goes through
            // `PvDatabase::process_record_with_links`, which resolves. The
            // claim is therefore "no caller of `process_local` has a
            // link-backed subscriber", and a caller that acquires one must
            // move to the link path rather than pass a backing here.
            let fields: Vec<String> = self.subscribers.keys().cloned().collect();
            for f in fields {
                self.notify_field_with_origin(
                    &f,
                    crate::server::recgbl::EventMask::PROPERTY,
                    0,
                    LinkBacking::none(),
                );
            }
        }

        if process_result == RecordProcessResult::AsyncPending {
            // Async: PACT stays set, no further processing this cycle
            // Don't clear processing flag (guard won't run — we leak it intentionally)
            std::mem::forget(_guard);
            return Ok((
                ProcessSnapshot {
                    changed_fields: Vec::new(),
                },
                Vec::new(),
            ));
        }
        if let RecordProcessResult::AsyncPendingNotify(fields) = process_result {
            // Intermediate notification (e.g. DMOV=0 at move start).
            // Unlike AsyncPending, we DO release the processing flag so
            // subsequent I/O Intr cycles can continue processing normally.
            self.common.time = crate::runtime::general_time::get_current();
            // Filter out fields that haven't actually changed, and update
            // MLST/last_posted for those that have. Each intermediate
            // post carries DBE_VALUE|DBE_LOG — C motor's mid-move
            // `db_post_events` calls use `DBE_VAL_LOG`
            // (motorRecord.cc:2606 DMOV, and every other do_work post);
            // no alarm transition ran on this pending pass.
            let mut changed_fields = Vec::new();
            for (name, val) in fields {
                let changed = match self.posted_value(&name) {
                    Some(prev) => prev != &val,
                    None => true,
                };
                if changed {
                    if name == "VAL" {
                        if let Some(f) = val.to_f64() {
                            self.put_coerced("MLST", EpicsValue::Double(f));
                            self.common.mlst = Some(f);
                        }
                    }
                    self.record_value_post(&name, val.clone());
                    changed_fields.push((name, val, EventMask::VALUE | EventMask::LOG));
                }
            }
            // _guard drops here, clearing the processing flag
            return Ok((ProcessSnapshot { changed_fields }, Vec::new()));
        }
        if process_result == RecordProcessResult::CompleteNoEmit {
            // The record accumulated this cycle without emitting (compress
            // `status == 1`). C `compressRecord.c:365` runs the completion
            // epilogue (udf clear, timestamp, monitor, FLNK) only on an emit
            // cycle (`if (status != 1)`), so a non-emitting cycle must publish
            // nothing — skip the epilogue and return an empty snapshot, exactly
            // as the production engine path does in `processing.rs`. This keeps
            // the emit-gate uniform across both process-dispatch paths so the
            // invariant holds by construction, not by "process_local never
            // produces it". CompleteNoEmit is synchronous (PACT already
            // cleared); the `_guard` drops here, clearing the processing flag.
            return Ok((
                ProcessSnapshot {
                    changed_fields: Vec::new(),
                },
                Vec::new(),
            ));
        }

        // `CompleteDeferOutput` (swait ODLY delay-start) is NOT special-cased
        // here: it deliberately shares the Complete value-side snapshot builder
        // below. C `swaitRecord.c::process` posts the value side (`monitor()`,
        // line 475) on the delaying cycle, so building the snapshot now is the
        // correct, parity-matching behavior — unlike `CompleteNoEmit` above,
        // whose fall-through would wrongly emit. The variant's *other* halves —
        // holding PACT across the delay and deferring OUT/OEVT/FLNK to the
        // `ReprocessAfter` continuation — are the engine path's responsibility
        // (`processing.rs::process_record_with_links_inner`); `process_local` is
        // a body-only test helper that dispatches no FLNK/output and no
        // `ProcessAction`, and no test drives a swait ODLY record through it. So
        // the invariant still holds by construction across both dispatch paths:
        // both publish the value side here, both leave the output side to the
        // engine.

        // UDF update before alarm evaluation — C parity (see
        // `processing.rs`). A NaN / undefined value keeps UDF true so
        // `recGblCheckUDF` raises UDF_ALARM this cycle instead of the
        // record reporting a stale/garbage value with no alarm.
        if self.record.clears_udf() {
            self.common.udf = self.record.value_is_undefined() as u8;
        }
        // Per-record alarm hook (C `checkAlarms()`).
        self.record.check_alarms(&mut self.common);

        // Evaluate alarms (accumulates into nsta/nsev)
        self.evaluate_alarms();

        // Transfer nsta/nsev → sevr/stat, detect alarm change
        let alarm_result = recgbl::rec_gbl_reset_alarms(&mut self.common);

        self.common.time = crate::runtime::general_time::get_current();
        // UDF already updated above — do not clear unconditionally.

        // Deadband check for VAL monitor filtering
        let (include_val, include_archive) = self.check_deadband_ext();
        // C `recGblResetAlarms` `val_mask = DBE_ALARM`
        // (recGbl.c:194/203/212): every monitored-value post this cycle
        // carries DBE_ALARM when the severity/status OR the alarm
        // message moved — same parity rule as the `processing.rs`
        // paths.
        let alarm_bits = if alarm_result.alarm_changed || alarm_result.amsg_changed {
            EventMask::ALARM
        } else {
            EventMask::NONE
        };

        // Build snapshot
        let mut changed_fields = Vec::new();
        // Same deadband-field routing and per-field mask as the
        // `processing.rs` paths: the tracked field posts the classes
        // that actually fired (MDEL → DBE_VALUE, ADEL → DBE_LOG, alarm
        // movement → DBE_ALARM); a non-primary deadband field (motor
        // RBV — C motor `monitor()`, motorRecord.cc:3468-3507) leaves
        // VAL to the generic change-detection loop below.
        let deadband_field = self.record.monitor_deadband_field();
        // The mask every change-detected aux field posts with — owned by
        // `AuxPostMask`, the same resolver the `processing.rs` paths use, so
        // this builder cannot drift from them on what mask a field carries.
        let aux_post = AuxPostMask::of(self.record.as_ref());
        // The deadband field's post — mask owned by `deadband_post`, the single
        // assembler C's `db_post_events(&prec->val, monitor_mask)` maps to.
        let deadband = self.deadband_post(alarm_bits, include_val, include_archive);
        let deadband_mask = deadband.mask;
        if let Some((field, value)) = deadband.field {
            changed_fields.push((field, value, deadband_mask));
        }
        // C `recGblResetAlarms` (recGbl.c:202-222) posts each alarm
        // field with its OWN per-field mask, not one record-wide mask:
        //   * SEVR — DBE_VALUE, ONLY on a sevr change.
        //   * STAT — DBE_ALARM (sevr change) | DBE_VALUE (stat change).
        //   * ACKS — DBE_VALUE, only when an alarm field moved.
        // Pushing SEVR/STAT into `changed_fields` collapses them onto
        // the single record-wide `event_mask` (which carries ALARM on
        // `alarm_changed`): a DBE_VALUE-only `.SEVR` subscriber would
        // miss a stat-only-driven sevr change, and a DBE_ALARM-only
        // `.SEVR` subscriber would be wrongly notified. Post them via
        // `notify_field` with their individual masks instead — exactly
        // as the `processing.rs` link path does.
        let sevr_changed = self.common.sevr != alarm_result.prev_sevr;
        let stat_changed = self.common.stat != alarm_result.prev_stat;
        let stat_mask = {
            let mut m = EventMask::NONE;
            // C `recGblResetAlarms` carries DBE_ALARM on the STAT/AMSG
            // posts whenever the severity OR the alarm message moved —
            // not on a severity change alone. Aligning with the
            // `processing.rs` link path (and `complete_async_record`).
            if sevr_changed || alarm_result.amsg_changed {
                m |= EventMask::ALARM;
            }
            if stat_changed {
                m |= EventMask::VALUE;
            }
            m
        };
        let mut alarm_posts: Vec<(&'static str, EventMask)> = Vec::new();
        if sevr_changed {
            alarm_posts.push(("SEVR", EventMask::VALUE));
        }
        if !stat_mask.is_empty() {
            alarm_posts.push(("STAT", stat_mask));
            // AMSG shares STAT's mask — C posts it alongside STAT when
            // any alarm field moved.
            alarm_posts.push(("AMSG", stat_mask));
        }
        // C parity (recGbl.c:214-217): ACKS is posted (DBE_VALUE) whenever the
        // alarm-acknowledge rule fires — `acks_posted` already folds in C's
        // `if (stat_mask)` guard, and the post carries no value-change test.
        if alarm_result.acks_posted {
            alarm_posts.push(("ACKS", EventMask::VALUE));
        }

        // The cycle's subscriber posts — assembled by the single owner
        // `collect_subscriber_posts`, shared with every `processing.rs` path.
        changed_fields.extend(self.collect_subscriber_posts(
            deadband_field,
            deadband_mask,
            alarm_bits,
            aux_post,
            include_val,
        ));
        // C waveform/aai/aao `monitor()` posts HASH with a literal
        // `DBE_VALUE` only on a content-hash change (waveformRecord.c:
        // 317-319), independent of the VAL post mask. `array_hash_changed`
        // was set by `check_deadband_ext` this cycle.
        if self.array_hash_changed {
            if let Some(h) = self.resolve_field("HASH") {
                changed_fields.push(("HASH".to_string(), h, EventMask::VALUE));
            }
        }

        // No `.UDF` post — C `monitor()` posts UDF nowhere, and
        // `recGblResetAlarms` (recGbl.c:202-222) posts only SEVR/STAT/AMSG/
        // ACKS. A `.UDF` event exists only where C's generic `dbPut` posts
        // the field it wrote (dbAccess.c:1411-1413) — i.e. a client caput to
        // `.UDF` itself.

        Ok((ProcessSnapshot { changed_fields }, alarm_posts))
    }

    /// **The single owner of "write a value into a record field in the type
    /// that field stores"** — a `put_field` arm binds ONE variant and silently
    /// drops the rest, and the trackers this writes differ in type per record:
    /// C declares LALM/ALST/MLST with the record's VAL type, `DBF_INT64` on
    /// int64in/int64out (`int64inRecord.dbd.pod:233-243`), `DBF_LONG` on
    /// longin/longout, `DBF_DOUBLE` elsewhere.
    ///
    /// Takes the value in the CALLER's domain rather than an `f64`: the alarm
    /// ladder's `alev` is an `epicsInt64` on the int64 records and going
    /// through a double would have rounded the very threshold LALM exists to
    /// remember.
    pub(crate) fn put_coerced(&mut self, field: &str, val: EpicsValue) {
        let target_type = self
            .record
            .get_field(field)
            .map(|v| v.db_field_type())
            .unwrap_or(crate::types::DbFieldType::Double);
        let coerced = val.convert_to(target_type);
        let _ = self.record.put_field(field, coerced);
    }

    /// Check MDEL/ADEL deadbands for VAL monitor/archive filtering.
    /// Returns `(monitor_trigger, archive_trigger)`.
    ///
    /// Updates `MLST`/`ALST` (record-owned) and the `CommonFields`
    /// `mlst/alst` shadow when a trigger fires. Records without
    /// MDEL/ADEL (e.g. motor) default to deadband=0 (any actual
    /// change triggers).
    ///
    /// Delegates the comparison to the free function [`check_deadband`]
    /// below, which ports C `recGblCheckDeadband` (recGbl.c:345-370).
    /// `None` there is "this record type carries no MLST/ALST cell and
    /// nothing has been posted yet", the only state C does not have.
    /// The single owner of the deadband field's monitor post — C `monitor()`'s
    /// `db_post_events(&prec->val, monitor_mask)`, the one post every record
    /// makes for the value it deadbands.
    ///
    /// [`Self::check_deadband_ext`] decides WHETHER the MDEL/ADEL classes fired;
    /// this decides what the resulting post looks like, and it is the only place
    /// that assembles that mask. The three `processing.rs` snapshot builders and
    /// the `notify_monitors` path all route through here, so a record's mask rule
    /// cannot hold on one processing path and not another.
    ///
    /// Two record hooks strip C's `DBE_LOG` from the post:
    ///
    /// * [`Record::value_only_change_fields`] — C posts a literal `DBE_VALUE`
    ///   (scaler VAL, scalerRecord.c:478).
    /// * [`Record::fields_posted_with_monitor_mask`] — C posts
    ///   `monitor_mask | DBE_VALUE` (event VAL, eventRecord.c:163). `monitor_mask`
    ///   there is `recGblResetAlarms`'s return, i.e. the alarm bits alone, so the
    ///   post carries `DBE_VALUE` (+ `DBE_ALARM` when the alarm moved) and never
    ///   the archive `DBE_LOG` — an event's VAL reaches a `DBE_LOG` archiver on
    ///   no cycle at all.
    ///
    /// [`DeadbandPost::field`] is `None` when no class fired, i.e. when C's
    /// `if (monitor_mask)` guard would skip the post.
    /// C `monitor()`'s VALUE / LOG gate for the primary-value post —
    /// `(include_val, include_archive)`, the single owner every processing path
    /// feeds into [`Self::deadband_post`] and [`Self::collect_subscriber_posts`].
    /// Keeping it in one place is what stops the rule from holding on the
    /// synchronous path but not the async-continuation / put-notify paths.
    pub(crate) fn value_include_classes(&mut self) -> (bool, bool) {
        // fanout/seq "trigger" records post VAL only with the alarm events
        // `recGblResetAlarms` returns, never DBE_VALUE/DBE_LOG — see
        // `Record::process_posts_value_monitor`. The alarm bits still reach VAL
        // via `deadband_post`'s `alarm_bits`, so an alarm transition still posts
        // it; only the value/archive classes are suppressed.
        if !self.record.process_posts_value_monitor() {
            return (false, false);
        }
        match self.record.monitor_value_changed() {
            // lsi/lso post VALUE|LOG only when the string actually changed (C
            // `lsiRecord.c`/`lsoRecord.c` monitor: `len != olen || memcmp(oval,
            // val, len)`); they have no MDEL/ADEL deadband to express that, so
            // the gate is explicit. The MPST/APST `menuPost` "Always" override
            // OR-adds DBE_VALUE / DBE_LOG even on an unchanged cycle (C monitor:
            // `if (mpst == menuPost_Always) events |= DBE_VALUE; if (apst ==
            // menuPost_Always) events |= DBE_LOG;`).
            Some(changed) => {
                let (val_always, archive_always) = self.record.monitor_always_post();
                (changed || val_always, changed || archive_always)
            }
            None => {
                if self.record.uses_monitor_deadband() {
                    self.check_deadband_ext()
                } else {
                    // Binary records (bi/bo/busy/mbbi/mbbo): always post monitors
                    (true, true)
                }
            }
        }
    }

    pub(crate) fn deadband_post(
        &self,
        alarm_bits: EventMask,
        include_val: bool,
        include_archive: bool,
    ) -> DeadbandPost {
        let field = self.record.monitor_deadband_field();
        let log_suppressed = self.record.value_only_change_fields().contains(&field)
            || self
                .record
                .fields_posted_with_monitor_mask()
                .contains(&field);

        let mut mask = alarm_bits;
        if include_val {
            mask |= EventMask::VALUE;
        }
        if include_archive && !log_suppressed {
            mask |= EventMask::LOG;
        }

        // The closed set applies to THIS post too. `process_posted_fields` is
        // "the CLOSED set of fields a process cycle of this record may post" —
        // and the deadband post is a post. A record whose C `monitor()` never
        // names the deadband field must not have one invented for it: transform
        // `monitor()` (transformRecord.c:786-809) walks A..P and posts no VAL
        // at all — VAL is an inert dummy (`:422`) — so an alarm cycle, whose
        // `alarm_bits` alone make `mask` non-empty, was firing a `.VAL` monitor
        // C never sends. Gating here rather than at each builder keeps the
        // single owner of the deadband post the single enforcer of the set.
        let in_closed_set = self
            .record
            .process_posted_fields()
            .is_none_or(|allowed| allowed.contains(&field));

        let value = if mask.is_empty() || !in_closed_set {
            None
        } else if field == "VAL" {
            self.record.val()
        } else {
            self.resolve_field(field)
        };
        DeadbandPost {
            mask,
            field: value.map(|v| (field.to_string(), v)),
        }
    }

    pub fn check_deadband_ext(&mut self) -> (bool, bool) {
        // C waveform/aai/aao `monitor()` (waveformRecord.c:291-326) replaces
        // the analog MDEL/ADEL deadband with the MPST/APST "Always vs On
        // Change" mechanism: the record hashes its array content and posts
        // `DBE_VALUE`/`DBE_LOG` either always or only when the hash changed,
        // and posts `HASH` (`DBE_VALUE`) on a hash change. The record owns
        // the hash compute + `HASH` update; `array_hash_changed` carries the
        // event to the snapshot builders, which post `HASH` (the field is
        // excluded from the generic change-detection loop via
        // `event_posted_fields`).
        if let Some(post) = self.record.array_monitor_post() {
            self.array_hash_changed = post.hash_changed;
            return (post.post_value, post.post_archive);
        }
        self.array_hash_changed = false;

        // The deadband is evaluated against `monitor_deadband_value()`,
        // not `val()` directly: a record whose monitored quantity is
        // not its primary value (e.g. the motor record, VAL=setpoint /
        // RBV=readback — C `monitor()` deadbands RBV) overrides that
        // hook. Default is `val()`, so other records are unaffected.
        let val = match self
            .record
            .monitor_deadband_value()
            .and_then(|v| v.to_f64())
        {
            Some(v) => v,
            None => return (true, true),
        };

        let mdel = self
            .record
            .get_field("MDEL")
            .and_then(|v| v.to_f64())
            .unwrap_or(0.0);
        let adel = self
            .record
            .get_field("ADEL")
            .and_then(|v| v.to_f64())
            .unwrap_or(0.0);

        // Use record's MLST/ALST fields if available, otherwise fall back to
        // CommonFields. `None` survives to `check_deadband` as the "nothing
        // posted yet" state: record types that carry no MLST/ALST cell (sel,
        // scalcout) have nowhere to hold a last-posted value.
        let mlst = self
            .record
            .get_field("MLST")
            .and_then(|v| v.to_f64())
            .or(self.common.mlst);
        let alst = self
            .record
            .get_field("ALST")
            .and_then(|v| v.to_f64())
            .or(self.common.alst);

        let monitor_trigger = check_deadband(val, mlst, mdel);
        let archive_trigger = check_deadband(val, alst, adel);

        if archive_trigger {
            self.put_coerced("ALST", EpicsValue::Double(val));
            self.common.alst = Some(val);
        }
        if monitor_trigger {
            self.put_coerced("MLST", EpicsValue::Double(val));
            self.common.mlst = Some(val);
        }

        (monitor_trigger, archive_trigger)
    }

    /// Build a Snapshot for a given value, populated with the record's display
    /// metadata and the link metadata the poster resolved for this batch. Uses
    /// the metadata cache so the populate cost is paid at most once per
    /// metadata-stable interval (cf. `cached_metadata`).
    ///
    /// There is deliberately no `backing`-less form. One existed, defaulting to
    /// [`LinkBacking::none`], and it made "nothing was resolved" the thing a
    /// caller says by saying nothing — which is how the `DBE_PROPERTY` sweep
    /// came to post `CALC.A` with the calc's own precision (see
    /// `link_backed_metadata_is_read_live.rs`). A caller with nothing to
    /// resolve still writes `LinkBacking::none()`, and then it is a claim a
    /// reviewer can see and check.
    ///
    /// The monitor path reaches the same one consumer the GET path does
    /// (`finish_field_snapshot` -> `route_field_metadata`), so it carried the
    /// same defect: measured on the wire, a `camonitor -s` on a `calc`'s `A`
    /// after `caput TARGET.PREC 4` with the source never processed printed
    /// `5.0` where C printed `5.0000`. The resolve cannot happen here — the
    /// post runs with the record's own lock held — so the caller that owns the
    /// process/put cycle resolves it at a point where no lock is held and
    /// hands it in.
    pub fn make_monitor_snapshot(
        &self,
        field: &str,
        value: EpicsValue,
        backing: LinkBacking<'_>,
    ) -> super::super::snapshot::Snapshot {
        // A monitor update is posted from the record's own change-detection
        // loop, which hands over the STORED variant. Project it onto the
        // field's declared type here, at the same owner the GET path and the
        // CA create-channel path use, or a client that was told `DBR_ENUM` at
        // create time would be posted a `DBR_SHORT` update.
        // The poster's obligation, checked rather than trusted: a link-backed
        // field carries its target's units/precision/limits, and only a caller
        // holding no record lock can resolve them. A `LinkBacking::none()`
        // here would silently serve the slot's C seed instead — the X2 defect
        // in its new clothes. Debug-only because it is a property of the call
        // graph, not of the data: every path either resolves or provably
        // posts no link-backed field, and the suite is what proves it.
        debug_assert!(
            !backing.is_unresolved()
                || self
                    .record
                    .link_backed_metadata_field(&field.to_ascii_uppercase())
                    .is_none(),
            "{}: monitor post of link-backed field {field} with nothing resolved \
             — the poster must call PvDatabase::resolve_link_backed_metadata \
             at a point where it holds no record lock",
            self.name
        );
        let value = self.project_to_declared_type(field, value);
        self.finish_field_snapshot(field, value, backing)
    }

    /// Apply a record's per-field metadata override (C RSET
    /// `get_units`/`get_precision`/`get_graphic_double`/
    /// `get_control_double`/`get_alarm_double`, all keyed by field)
    /// over the cached record-level metadata. Shared by the GET and
    /// monitor snapshot builders. Computed live on every call — never
    /// cached — so overrides derived from fields outside the
    /// [`is_metadata_cache_source`] set cannot go stale.
    ///
    /// This is also where the record-level `Q:form` info tag is narrowed to
    /// the served field: QSRV assigns `display.form.index` only when the
    /// channel addresses the VAL field (`IOCSource::initialize` gates it on
    /// `dbIsValueField(dbChannelFldDes(chan))`, `iocsource.cpp:53`; the form
    /// *menu*, `form.choices`, is published for every field). The metadata
    /// cache is per-record, so a channel on `REC.RVAL` of a record carrying
    /// `info(Q:form, "Hex")` used to report Hex where pvxs reports Default.
    /// Both `Snapshot` producers (`snapshot_for_field` for GET,
    /// `make_monitor_snapshot` for updates) run this one owner, so
    /// `DisplayInfo::form` means exactly one thing on every path: the form
    /// index that applies to THIS field.
    fn apply_field_metadata_override(
        &self,
        field: &str,
        snap: &mut super::super::snapshot::Snapshot,
    ) {
        if let Some(display) = snap.display.as_mut()
            && !crate::server::database::is_value_field(field)
        {
            display.form = 0;
        }
        let Some(ov) = self.record.field_metadata_override(field) else {
            return;
        };
        if ov.units.is_some()
            || ov.precision.is_some()
            || ov.disp_limits.is_some()
            || ov.alarm_limits.is_some()
        {
            let d = snap.display.get_or_insert_with(Default::default);
            if let Some(units) = ov.units {
                d.units = units;
            }
            if let Some(precision) = ov.precision {
                d.precision = precision;
            }
            if let Some((upper, lower)) = ov.disp_limits {
                d.upper_disp_limit = upper;
                d.lower_disp_limit = lower;
            }
            if let Some((hihi, high, low, lolo)) = ov.alarm_limits {
                d.upper_alarm_limit = hihi;
                d.upper_warning_limit = high;
                d.lower_warning_limit = low;
                d.lower_alarm_limit = lolo;
            }
        }
        if let Some((upper, lower)) = ov.ctrl_limits {
            let c = snap.control.get_or_insert_with(Default::default);
            c.upper_ctrl_limit = upper;
            c.lower_ctrl_limit = lower;
        }
    }

    /// C's rset metadata slots route **per field**, on `dbGetFieldIndex`. The
    /// port's metadata cache is the record's VAL metadata, and serving it to
    /// every field is what made a non-VAL field report VAL's limits.
    ///
    /// Every base record's `get_control_double` / `get_alarm_double` has the
    /// same two-arm shape: a listed set of field indices that take the
    /// record's own limits, and a `default:` arm that hands the field to
    /// `recGblGetControlDouble` / `recGblGetAlarmDouble` — the field TYPE's
    /// numeric range, and four NaN. This routes the `default:` arm; a listed
    /// field keeps the cache, which already holds exactly the record's own
    /// limits (and already distinguishes `ao`'s DRVH/DRVL from `ai`'s
    /// HOPR/LOPR).
    ///
    /// The three slots' listed sets are **different**, and each has its own
    /// owner here: [`Self::control_explicit_field`],
    /// [`Self::graphic_explicit_field`] and [`Self::alarm_explicit_field`].
    /// They are separate switches over separate field lists in C, so the
    /// membership question is asked once per slot, never once for both — and
    /// each list varies by record TYPE, so it is asked once per type too.
    ///
    /// Measured on a real `softIocPVX` against `record(calc,"X"){}`:
    /// `.PHAS` (DBF_SHORT, unlisted) serves control ±32767 — the SHRT range —
    /// while `.VAL` and `.HIHI` (both listed) serve 0/0 from HOPR/LOPR.
    ///
    /// Display (graphic) limits differ from control in one way: the `default:`
    /// arm of `get_graphic_double` tries a LINK first (`calcRecord.c`
    /// `get_linkNumber` → `dbGetGraphicLimits`) and only falls to `recGbl` for
    /// a field that backs no link. A constant (unset) link has no metadata
    /// getters, so the `dbAccess.c:216` 0/0 seed stands — measured: `CALC.A`
    /// serves display 0/0 but control ±1e300. [`Record::link_backed_metadata_field`]
    /// carries that per-record C knowledge.
    ///
    /// Units and precision are routed here too, and they are NOT the
    /// `get_*_double` shape: neither has a `recGbl` range arm, so a field the
    /// type's switch does not name keeps `dbAccess.c`'s memset — empty units,
    /// and the precision seed. They were built into the record-level cache
    /// until `subArray`/`sel`/`sub`/`dfanout` were measured serving `""`/`0`
    /// against C's EGU/PREC, which is the same dual meaning the alarm leaves
    /// had: whether a field carried the record's own value depended on a
    /// `match rtype` in a different function.
    ///
    /// The last arm is not the same for every record type — a slot can also
    /// fall through WITHOUT delegating, keeping the seed. That fact is one bit
    /// per record type, read from its C source: `control_default_arm`.
    fn route_field_metadata(
        &self,
        field: &str,
        backing: LinkBacking<'_>,
        snap: &mut super::super::snapshot::Snapshot,
    ) {
        // The rset slots this record type actually supplies. A NULL slot makes
        // `dbAccess.c` clear the option bit, so the leaf is never served and
        // there is nothing to route — minting a value here would put a
        // fabricated number into the struct while `Snapshot::properties` says
        // the slot is absent, and the two wires read different halves of that
        // disagreement: CA goes through the mask-gated accessors
        // (`codec.rs::get_limits` calls `graphic_limits()`/`alarm_limits()`/
        // `control_limits()`, each `then_some`-gated), PVA reads the struct
        // straight (`native_source.rs:226`) under its own leaf mask.
        let slots = self.record.property_support();
        let rtype = self.record.record_type();
        let f = field.to_ascii_uppercase();

        // C's `get_linkNumber` question, asked ONCE per snapshot: which of this
        // record's own link fields, if any, supplies this field's metadata.
        // Four of the six slots consult it — `aSubRecord.c:306-404` is the
        // complete specimen — and control does not, because
        // `dbGetControlLimits` has no caller anywhere in base.
        let link_backed = self.record.link_backed_metadata_field(&f);
        // Resolved for THIS build and handed in — see [`LinkBacking`]. Reading
        // a value the record had stored is what made a `caput SRC.EGU` invisible
        // to a passive source's clients until the source next processed.
        let link_meta = link_backed.as_ref().and_then(|lf| backing.metadata(lf));

        // C `get_units`'s "no case" arm — the field the rset tests for and
        // declines to write, leaving `dbAccess.c:378`'s zeroed buffer. The
        // record-level cache holds `EGU` for every type that supplies the
        // slot, so this is the step that takes it back off the fields C never
        // gives it to. See [`Self::units_from_egu`].
        if slots.units {
            if link_backed.is_some() {
                // C's link arm — `dbGetUnits(&prec->inpa + n, ...)`, which
                // writes only what the TARGET record supplies. A constant or
                // unresolved link supplies nothing and `dbAccess.c:378`'s
                // zeroed buffer stands.
                snap.display.get_or_insert_with(Default::default).units = link_meta
                    .and_then(|m| m.units.as_deref())
                    .map(crate::types::PvString::from)
                    .unwrap_or_default();
            } else if !self.units_from_egu(rtype, &f) {
                snap.display.get_or_insert_with(Default::default).units = Default::default();
            }
        }

        // C `get_precision`'s link arm, which the port had no arm for at all.
        // All five link-routing types seed `*pprecision = prec->prec` — the
        // record's own PREC, already in the metadata cache — and overwrite it
        // only when `dbGetPrecision` on the backing link SUCCEEDS
        // (`calcRecord.c:184-203`, `aSubRecord.c:323-348`). So an unresolved or
        // constant link means "leave the cache alone", which is the `None`
        // arm here.
        if slots.precision {
            let link_precision = link_meta.and_then(|m| m.precision);
            if link_backed.is_some()
                && let Some(precision) = link_precision
            {
                snap.display.get_or_insert_with(Default::default).precision = precision;
            }
            // C `get_precision`'s SHARED TAIL — `recGblGetPrec`
            // (`recGbl.c:119-144`), which every one of these bodies hands the
            // fields it did not name. For a field that can carry a precision
            // (`dbAccess.c:388-389` gates `DBR_PRECISION` on
            // `DBF_FLOAT`/`DBF_DOUBLE`) the tail does one thing: clamp a PREC
            // outside `0..=15` to 15. Applied here rather than per record so
            // "does this field take the tail" has one owner —
            // [`Self::precision_explicit_field`] — instead of thirty
            // `field_metadata_override`s that each have to remember it.
            //
            // Gated on the field being float or double so the tail runs
            // exactly where C's does: `dbAccess.c:388-389` refuses to call
            // `get_precision` at all for any other type, which is what keeps
            // `recGblGetPrec`'s integer arm (`*precision = 0`) unobservable —
            // it must stay unobservable here too.
            let field_type = self.static_field_type(field);
            if matches!(
                field_type,
                Some(crate::types::DbFieldType::Float | crate::types::DbFieldType::Double)
            ) && !Self::precision_explicit_field(
                rtype,
                &f,
                link_backed.is_some(),
                link_precision.is_some(),
            ) {
                let d = snap.display.get_or_insert_with(Default::default);
                d.precision = crate::server::recgbl::rec_gbl_get_prec(field_type, d.precision);
            }
        }

        // C `get_control_double`'s last arm. No base record routes control
        // through a link: `dbGetControlLimits` has zero callers in all of
        // base, so unlike display this arm needs no link branch.
        if slots.control_double && !Self::control_explicit_field(rtype, field) {
            let (upper, lower) =
                match super::record_trait::control_default_arm(self.record.record_type()) {
                    // `recGblGetControlDouble` → `getMaxRangeValues(field_type)`.
                    // A type with no case in C's switch (STRING/MENU/DEVICE/links)
                    // is written by nothing, leaving the `dbAccess.c:256` seed —
                    // which is 0/0, exactly what `unwrap_or` supplies.
                    super::record_trait::RsetDefaultArm::RecGblRange => {
                        self.rec_gbl_range_for(field).unwrap_or((0.0, 0.0))
                    }
                    // The slot exists but writes nothing here, so the same
                    // `dbAccess.c:256` seed stands. Modelled as a value rather
                    // than as `None`: C's option bit is ON (the slot is supplied
                    // and returned 0), so the leaf IS served — carrying the seed.
                    super::record_trait::RsetDefaultArm::Seed => (0.0, 0.0),
                };
            snap.control = Some(super::super::snapshot::ControlInfo {
                upper_ctrl_limit: upper,
                lower_ctrl_limit: lower,
            });
        }

        // C `get_graphic_double`'s last arm. Unlike control this one has a LINK
        // branch ahead of the recGbl call, so the three answers are: keep the
        // cache (listed on HOPR/LOPR), the link's limits, or the default arm.
        if slots.graphic_double && !Self::graphic_explicit_field(rtype, field) {
            let (upper, lower) = if link_backed.is_some() {
                // `dbGetGraphicLimits` on the backing link. A CONSTANT link has
                // no metadata getters and an unresolved one has nothing cached,
                // so in both cases the `dbAccess.c:216` 0/0 seed stands.
                link_meta
                    .and_then(|m| m.graphic_limits)
                    .map(|(lower, upper)| (upper, lower))
                    .unwrap_or((0.0, 0.0))
            } else {
                match super::record_trait::graphic_default_arm(rtype) {
                    super::record_trait::RsetDefaultArm::RecGblRange => {
                        self.rec_gbl_range_for(field).unwrap_or((0.0, 0.0))
                    }
                    super::record_trait::RsetDefaultArm::Seed => (0.0, 0.0),
                }
            };
            let d = snap.display.get_or_insert_with(Default::default);
            d.upper_disp_limit = upper;
            d.lower_disp_limit = lower;
        }

        // C `get_alarm_double`, BOTH arms. This branch owns the four limits
        // outright: `slots.alarm_double` is exactly the condition under which
        // `getProperties` assigns the four `valueAlarm.*Limit` leaves, so
        // whenever the leaves are served this assigns them.
        //
        // The explicit arm used to be left to the record-level metadata cache
        // (`populate_display_info`), whose `match rtype` covered only some of
        // the types that supply the slot. A type it missed reached the wire
        // with `snap.display == None` and the four leaves kept the NT's
        // structural 0 — measured: DFANOUT.VAL, SEL.VAL and SUB.VAL served 0
        // where C serves NaN. That made "which limits does VAL carry" depend on
        // a match arm existing somewhere else, which is the dual meaning this
        // single owner removes.
        if slots.alarm_double {
            let (hihi, high, low, lolo) = if Self::alarm_explicit_field(rtype, field) {
                self.explicit_alarm_limits(rtype)
            } else if link_backed.is_some() {
                // `dbGetAlarmLimits` on the backing link; `dbAccess.c:294`'s
                // four NaN stand when it supplies nothing.
                link_meta
                    .and_then(|m| m.alarm_limits)
                    .map(|(lolo, low, high, hihi)| (hihi, high, low, lolo))
                    .unwrap_or_else(crate::server::recgbl::rec_gbl_get_alarm_double)
            } else {
                crate::server::recgbl::rec_gbl_get_alarm_double()
            };
            // The four alarm limits live on DisplayInfo because that mirrors
            // C's `dbr_gr_double` packing, which the CA encoder depends on.
            // Minting it here is safe: every other DisplayInfo field defaults
            // to the same value the `None` path already served.
            let d = snap.display.get_or_insert_with(Default::default);
            d.upper_alarm_limit = hihi;
            d.upper_warning_limit = high;
            d.lower_warning_limit = low;
            d.lower_alarm_limit = lolo;
        }
    }

    /// The four limits C's `get_alarm_double` serves for the fields its rset
    /// lists — [`alarm_explicit_fields`](super::record_trait::alarm_explicit_fields).
    ///
    /// Read through [`Self::resolve_field`], the same unified accessor C's
    /// `prec->hihi` is. The port stores the eight alarm fields in one of two
    /// disjoint homes — `common.analog_alarm` for the types with the analog
    /// ladder (`ai`/`ao`/`calc`/…), the record's own struct for the types
    /// without it (`dfanout`/`sel`) — and `resolve_field` spans both. Reading
    /// the ladder slot directly instead would answer NaN for every `dfanout`
    /// and `sel` no matter how its HIHI/HHSV were set, because those two types
    /// have no slot at all.
    fn explicit_alarm_limits(&self, rtype: &str) -> (f64, f64, f64, f64) {
        let limit = |name: &str| {
            self.resolve_field(name)
                .and_then(|v| v.to_f64())
                .unwrap_or(0.0)
        };
        // The raw stored ordinal, NOT clamped to 0..=3: C tests `prec->hhsv`
        // for NONZERO, so an out-of-range severity still enables its limit.
        let severity = |name: &str| {
            self.resolve_field(name)
                .and_then(|v| v.to_f64())
                .unwrap_or(0.0) as i16
        };
        match super::record_trait::alarm_val_arm(rtype) {
            super::record_trait::AlarmValArm::Unconditional => {
                (limit("HIHI"), limit("HIGH"), limit("LOW"), limit("LOLO"))
            }
            super::record_trait::AlarmValArm::Gated => (
                gated(severity("HHSV"), limit("HIHI")),
                gated(severity("HSV"), limit("HIGH")),
                gated(severity("LSV"), limit("LOW")),
                gated(severity("LLSV"), limit("LOLO")),
            ),
        }
    }

    /// The fields C's **`get_control_double`** answers with the record's own
    /// cached limits, rather than letting them fall to the `default:` arm.
    ///
    /// "VAL plus the seven alarm bands" is one type's list, not the shared
    /// one: it holds for `ai` (`aiRecord.c:267-288`), `ao`, `calc`, `calcout`,
    /// `longin`, `longout`, `int64in`, `int64out` and `sub`
    /// (`subRecord.c:272-292`) — the `_` arm — and for no other type. Every
    /// list below is transcribed from that type's own rset:
    ///
    /// * `aSub` (`aSubRecord.c:372-376`) is a bare `recGblGetControlDouble`:
    ///   it lists NOTHING, VAL included.
    /// * `seq` (`seqRecord.c:342-353`) lists only DLYn and `bo`
    ///   (`boRecord.c:310-318`) only HIGH — and both answer a LITERAL rather
    ///   than the cache, so they come from
    ///   [`Record::field_metadata_override`] (which runs after this routing
    ///   and wins over the `default:` arm). Nothing of these two types keeps
    ///   the cache, VAL included.
    /// * `dfanout` (`dfanoutRecord.c:197-213`) lists VAL and the three
    ///   latches but NOT the four bands.
    /// * `sel` (`selRecord.c:203-235`) lists the eight plus `A`..`L` /
    ///   `LA`..`LL`; `acalcout`/`scalcout` (`aCalcoutRecord.c:793-822`,
    ///   `sCalcoutRecord.c:653-682`) list VAL and the four bands but NOT the
    ///   latches, plus `A`..`L` / `PA`..`PL`.
    /// * `epid` (`epidRecord.c:263-287`) lists VAL, the four bands and CVAL on
    ///   HOPR/LOPR; `motor` (`motorRecord.cc:3263-3308`) lists VAL and RBV on
    ///   HLM/LLM.
    /// * the array types (`waveformRecord.c:268-289`, `aaiRecord.c:287-304`,
    ///   `aaoRecord.c:292-309`, `compressRecord.c:487-502`,
    ///   `histogramRecord.c:458-475`, `subArrayRecord.c:258-287`) list VAL
    ///   alone on the cache — their other listed fields answer computed spans,
    ///   so those too come from [`Record::field_metadata_override`].
    ///
    /// Fields whose listed case answers something OTHER than the record's
    /// cached limits are deliberately absent — `motor`'s DVAL/DRBV (DHLM/DLLM)
    /// and `epid`'s OVAL/P/I/D (DRVH/DRVL) have no override yet and so still
    /// take the `default:` arm.
    ///
    /// **Not** the other two slots' lists — see [`Self::alarm_explicit_field`]
    /// (smaller) and [`Self::graphic_explicit_field`] (larger, and cut short
    /// for different types). C's three rset arms are separate switches over
    /// separate field lists, so one shared predicate could only ever be right
    /// for one of them.
    fn control_explicit_field(rtype: &str, field: &str) -> bool {
        // The types that list nothing the cache can answer, VAL included.
        //
        // `tableRecord.c:795-810` and `mcaRecord.c:929-943` are the same
        // shape as `aSub`: a small named set (table's six user coordinates
        // `AX`..`Z`, mca's dead `BPTR` arm) and `recGblGetControlDouble` for
        // everything else — VAL and the alarm bands included. Both named sets
        // answer a literal rather than the record's HOPR/LOPR, so they come
        // from [`Record::field_metadata_override`] and nothing here keeps the
        // cache.
        if matches!(rtype, "aSub" | "seq" | "bo" | "table" | "mca") {
            return false;
        }
        if crate::server::database::is_value_field(field) {
            return true;
        }
        let f = field.to_ascii_uppercase();
        let bands: &[&str] = match rtype {
            "dfanout" => &["LALM", "ALST", "MLST"],
            "acalcout" | "scalcout" | "epid" => &["HIHI", "HIGH", "LOW", "LOLO"],
            "waveform" | "aai" | "aao" | "compress" | "histogram" | "subArray" | "motor" => &[],
            _ => &["HIHI", "HIGH", "LOW", "LOLO", "LALM", "ALST", "MLST"],
        };
        if bands.contains(&f.as_str()) {
            return true;
        }
        match rtype {
            // sel's args are 12 (`SEL_MAX`), not the calc family's 21.
            "sel" => Self::calc_arg_field(&f, 12),
            "acalcout" | "scalcout" => {
                Self::calc_arg_field(&f, 12)
                    || matches!(f.as_bytes(), [b'P', c] if c.is_ascii_uppercase() && *c <= b'L')
            }
            "epid" => f == "CVAL",
            "motor" => f == "RBV",
            _ => false,
        }
    }

    /// The fields C's **`get_alarm_double`** lists explicitly — **VAL alone**,
    /// not the eight [`Self::control_explicit_field`] lists.
    ///
    /// Transcribed from every rset in base that supplies the slot. Most are a
    /// bare `if (dbGetFieldIndex(paddr) == indexof(VAL))` with every other
    /// field falling to `recGblGetAlarmDouble` (`recGbl.c:155-162`, four NaN):
    /// `aiRecord.c:294`, `aoRecord.c:368`, `dfanoutRecord.c:218`,
    /// `int64inRecord.c:239`, `int64outRecord.c:283`, `longinRecord.c:244`,
    /// `longoutRecord.c:300`, `selRecord.c:241`. Three do NOT have that shape
    /// and reach the same NaN for a band field the long way —
    /// `calcRecord.c:257-280`, `calcoutRecord.c:532-555` and
    /// `subRecord.c:294-317` hoist the index into a `fieldIndex` local, test
    /// VAL, and otherwise try `get_linkNumber` first, so only a field that is
    /// neither VAL nor an `INPx` slot falls through to `recGblGetAlarmDouble`
    /// (`subRecord.c:313-314`); an `INPx` field takes that LINK's alarm limits
    /// through `dbGetAlarmLimits`, not the four NaN.
    ///
    /// So `.HIHI` serves VAL's *control* limits but NOT VAL's *alarm* limits —
    /// the band fields' four alarm limits are the recGbl NaN. Routing both
    /// slots off one VAL-class predicate is what put the record's own
    /// valueAlarm limits on all eight.
    ///
    /// Which fields each type lists — and the fact that some list none, and
    /// that `motor` lists two — is one per-type table,
    /// [`alarm_explicit_fields`](super::record_trait::alarm_explicit_fields);
    /// what that listed arm ANSWERS is its twin,
    /// [`alarm_val_arm`](super::record_trait::alarm_val_arm). Keeping the two
    /// questions in one place is what lets this predicate stay a pure
    /// membership test.
    fn alarm_explicit_field(rtype: &str, field: &str) -> bool {
        super::record_trait::alarm_explicit_fields(rtype)
            .iter()
            .any(|f| field.eq_ignore_ascii_case(f))
    }

    /// `A`..`A+n-1` (a single letter) or `LA`..`LA+n-1` — C's calc-family
    /// argument fields, addressed by index range rather than by name.
    ///
    /// `calcRecord.c:161-167` / `calcoutRecord.c:417-423` test
    /// `idx >= indexof(A) && idx < indexof(A) + CALCPERFORM_NARGS`, and the dbd
    /// declares those `CALCPERFORM_NARGS` fields contiguously as the single
    /// letters `A`..`U` (`postfix.h:29` = 21, `calcRecord.dbd.pod:801-985`), so
    /// the index range and the letter range are the same set.
    fn calc_arg_field(field: &str, nargs: u8) -> bool {
        let last = b'A' + nargs - 1;
        match field.as_bytes() {
            [c] => c.is_ascii_uppercase() && *c <= last,
            [b'L', c] => c.is_ascii_uppercase() && *c <= last,
            _ => false,
        }
    }

    /// The fields C's **`get_graphic_double`** answers with the record's own
    /// `HOPR`/`LOPR` — which is exactly what the VAL metadata cache already
    /// holds, so routing must leave them on it.
    ///
    /// The third membership question, and a third distinct set: the alarm arm
    /// lists VAL alone and the control arm lists the eight, but graphic lists
    /// the eight PLUS a per-type tail, and two types cut it short.
    ///
    /// * base analog (`aiRecord.c:244-265`, `aoRecord.c:316-339`,
    ///   `calcRecord.c:187-212`, `calcoutRecord.c:452-484`,
    ///   `subRecord.c:242-270`, `selRecord.c:181-201`,
    ///   `dfanoutRecord.c:181-195`, `longinRecord.c:190-204`,
    ///   `longoutRecord.c`, `int64inRecord.c:196-210`, `int64outRecord.c`):
    ///   the eight.
    /// * `acalcout`/`scalcout` (`aCalcoutRecord.c:1046`, `sCalcoutRecord.c:906`)
    ///   list only VAL/HIHI/HIGH/LOW/LOLO — NOT LALM/ALST/MLST — plus the
    ///   `A`..`L` and `PA`..`PL` ranges.
    /// * `sel` (`selRecord.c:193-196`) also lists `A`..`L` / `LA`..`LL`, via a
    ///   GCC case range. It has no link arm at all, so its args are HOPR/LOPR
    ///   where calc's identically-named ones are link-backed.
    /// * the SVAL family (`aiRecord.c:253`, `longinRecord.c`,
    ///   `int64inRecord.c:205`), `ao`'s `OVAL`/`PVAL`/`IVOV`
    ///   (`aoRecord.c:322-338`), and `compress`'s `IHIL`/`ILIL`
    ///   (`compressRecord.c:474-476`).
    ///
    /// Fields whose graphic case answers something OTHER than HOPR/LOPR are
    /// NOT here — they cannot keep the cache and are supplied by
    /// [`Record::field_metadata_override`] instead (`histogram` WDTH,
    /// `subArray`/`waveform`/`aai`/`aao` index fields, `seq` DLYn,
    /// `calcout` ODLY).
    fn graphic_explicit_field(rtype: &str, field: &str) -> bool {
        // The two types that do not list VAL. Neither switch is keyed on
        // VAL at all: `seqRecord.c:282-297` keys on `index - indexof(DLY0)`,
        // so every field BELOW DLY0 — VAL included — reaches
        // `recGblGetGraphicDouble`; `aSubRecord.c:350-368` keys on the link
        // number, and VAL is neither an inlink nor an outlink, so it falls out
        // having written nothing (the `graphic_default_arm` Seed).
        //
        // Measured: `SEQ.VAL` served display 0/0 — the empty VAL cache — where
        // C serves the DBF_LONG range.
        //
        // `tableRecord.c:778-792` and `mcaRecord.c:910-927` key on a named set
        // too — table's `AX`..`Z` window, mca's `DTIM`/`IDTIM` percent scale
        // and dead `BPTR` arm — and hand every other field, VAL included, to
        // `recGblGetGraphicDouble`. Both named sets answer literals through
        // [`Record::field_metadata_override`], so neither type keeps the cache.
        if matches!(rtype, "seq" | "aSub" | "table" | "mca") {
            return false;
        }
        if crate::server::database::is_value_field(field) {
            return true;
        }
        let f = field.to_ascii_uppercase();
        let bands: &[&str] = match rtype {
            "acalcout" | "scalcout" | "epid" => &["HIHI", "HIGH", "LOW", "LOLO"],
            // `swaitRecord.c:597-606` is a bare `pfield == &pwait->val` test,
            // so its `ALST`/`MLST` take `recGblGetGraphicDouble` — and swait
            // has no HIHI/HIGH/LOW/LOLO to ask about.
            "swait" => &[],
            _ => &["HIHI", "HIGH", "LOW", "LOLO", "LALM", "ALST", "MLST"],
        };
        if bands.contains(&f.as_str()) {
            return true;
        }
        match rtype {
            "ai" | "longin" | "int64in" => f == "SVAL",
            "ao" => matches!(f.as_str(), "OVAL" | "PVAL" | "IVOV"),
            "compress" => matches!(f.as_str(), "IHIL" | "ILIL"),
            // sel's args are 12 (`SEL_MAX`), not the calc family's 21.
            "sel" => Self::calc_arg_field(&f, 12),
            // A..L and PA..PL, both to HOPR/LOPR.
            "acalcout" | "scalcout" => {
                Self::calc_arg_field(&f, 12)
                    || matches!(f.as_bytes(), [b'P', c] if c.is_ascii_uppercase() && *c <= b'L')
            }
            // `epidRecord.c:238-248` names CVAL alongside VAL and the four
            // bands — the same list its `get_control_double` (`:263-273`) has
            // and this predicate did not.
            "epid" => f == "CVAL",
            _ => false,
        }
    }

    /// The fields C's **`get_precision`** answers without reaching
    /// `recGblGetPrec` — the fourth membership question, and a fourth distinct
    /// set.
    ///
    /// Every `get_precision` in base and in the ported modules is the same
    /// two-part body: name some fields and answer them outright, hand the rest
    /// to `recGblGetPrec` (`recGbl.c:119-144`). For a field that can carry a
    /// precision at all — `dbAccess.c:388-389` gates `DBR_PRECISION` on
    /// `DBF_FLOAT`/`DBF_DOUBLE` — that shared tail does exactly one thing:
    /// clamp an out-of-range `PREC` to 15. So this predicate is what decides
    /// whether `caput REC.PREC 20` reaches a client as 20 or as 15, and the
    /// two answers differ per FIELD within one record: `ai.VAL` returns before
    /// the tail and serves 20, `ai.HOPR` falls into it and serves 15
    /// (`aiRecord.c:234-242`).
    ///
    /// The literal arms (`bo.HIGH`, `seq.DLYn`, `calcout.ODLY`,
    /// `histogram.SDEL`, `motor.VERS`, …) need no entry: they are
    /// [`Record::field_metadata_override`]s, which run after this and win, and
    /// every literal C uses is already inside `0..=15`.
    ///
    /// `link_supplied` is `dbGetPrecision`'s status on the backing link, and
    /// only `seq` reads it — see the `link_backed` arm.
    fn precision_explicit_field(
        rtype: &str,
        field: &str,
        link_backed: bool,
        link_supplied: bool,
    ) -> bool {
        match rtype {
            // No `recGblGetPrec` in the body at all: every field keeps PREC,
            // `ODLY` its literal 3 (`swaitRecord.c:583-595`).
            "swait" => return true,
            // `if (fieldIndex == VERS) 2; else if (fieldIndex >= VAL) prec;
            // else recGblGetPrec(...) /* Field is in dbCommon */`
            // (`transformRecord.c:752-767`, `scalerRecord.c:728-741`,
            // `tableRecord.c:814-828`). Only fields BELOW `VAL` — dbCommon —
            // reach the tail, and dbCommon declares no `DBF_FLOAT`/`DBF_DOUBLE`
            // field, so nothing that can be served ever gets there.
            "transform" | "scaler" | "table" => return true,
            // The same split inverted: `if (pfield < &pR->val) return 0;` then
            // `recGblGetPrec` (`sseqRecord.c:810-822`). Here it is the RECORD's
            // own fields — every `DLYn`, i.e. everything that can be served —
            // that reaches the tail, and the exempt half is the dbCommon one
            // that cannot.
            "sseq" => return false,
            // Falls through on every field, `DLY` included: it takes `DPREC`
            // instead of `PREC` and is clamped anyway
            // (`throttleRecord.c:451-464`).
            "throttle" => return false,
            _ => {}
        }
        if link_backed {
            // `if (linkNumber >= 0) { if (dbGetPrecision(...) == 0) *p = ...; }
            // else recGblGetPrec(...)` — the link arm returns whether or not
            // the link answered (`calcRecord.c:194-201`,
            // `calcoutRecord.c:461-468`, `subRecord.c:231-238`,
            // `aSubRecord.c:330-346`).
            //
            // `seq` is the exception, and it is why this takes a second
            // argument: its `case 2:` returns ONLY when `dbGetPrecision`
            // succeeded, and a `DOn` over a constant `DOLn` falls out of the
            // switch into the shared tail (`seqRecord.c:310-317`).
            return if rtype == "seq" { link_supplied } else { true };
        }
        if crate::server::database::is_value_field(field) {
            // `*precision = prec->prec; if (VAL) return 0;` — the common
            // shape (`aiRecord.c:238-239`, `aaiRecord.c:262-264`,
            // `aaoRecord.c:267-269`, `aoRecord.c:304-312`,
            // `calcRecord.c:190-192`, `calcoutRecord.c:457-459`,
            // `compressRecord.c:464-466`, `dfanoutRecord.c:169-171`,
            // `selRecord.c:152-155`, `subArrayRecord.c:221-223`,
            // `subRecord.c:227-229`, `waveformRecord.c:239-241`,
            // `sCalcoutRecord.c:616-618`, `aCalcoutRecord.c:756-758`,
            // `epidRecord.c:230-233`).
            //
            // The five that do NOT name VAL: `aSub` keys on the link number
            // only and VAL is neither an inlink nor an outlink
            // (`aSubRecord.c:330-346`); `mca` names `BPTR` and the four
            // calibration fields (`mcaRecord.c:898-905`); `seq` keys on
            // `index - indexof(DLY0)`, leaving VAL below the switch
            // (`seqRecord.c:305-317`); `histogram`'s switch has no VAL case
            // (`histogramRecord.c:423-436`); `motor` reaches the tail from
            // `default:` (`motorRecord.cc:3319-3335`).
            return !matches!(rtype, "aSub" | "mca" | "seq" | "histogram" | "motor");
        }
        let f = field.to_ascii_uppercase();
        match rtype {
            // `case VAL: case OVAL: case PVAL: break;` (`aoRecord.c:305-312`).
            "ao" => matches!(f.as_str(), "OVAL" | "PVAL"),
            // `if (fieldIndex == VAL || fieldIndex == CVAL) return 0;`
            // (`epidRecord.c:231-232`).
            "epid" => f == "CVAL",
            // The five cases that answer `prec->prec`
            // (`histogramRecord.c:424-430`). `SDEL` is the sixth case and a
            // literal, so the override covers it. Note that histogram's tail
            // gets an UNSEEDED `precision` — the record never assigns
            // `prec->prec` before the switch — so C answers `dbAccess.c:387`'s
            // zeroed buffer there, not a clamped PREC; `SDLY` is histogram's
            // only such field and carries its own `Some(0)` override.
            "histogram" => matches!(f.as_str(), "ULIM" | "LLIM" | "SGNL" | "SVAL" | "WDTH"),
            // `BPTR` returns, and the four calibration fields answer a literal
            // 6 (`mcaRecord.c:898-905`).
            "mca" => matches!(f.as_str(), "BPTR" | "CALO" | "CALS" | "CALQ" | "TTH"),
            // `case RRBV: case RMP: case REP: *precision = 0; break;` and
            // `case VERS: *precision = 2; break;` — both `break` past the
            // switch to the bare `return`, never to `recGblGetPrec`
            // (`motorRecord.cc:3322-3330`).
            "motor" => matches!(f.as_str(), "RRBV" | "RMP" | "REP" | "VERS"),
            // `sel` is deliberately absent: its `A`..`L` / `LA`..`LL` loop
            // compares `paddr->pfield` against `&pvalue` and `&plvalue` — the
            // addresses of the two LOCAL pointers, not the fields they walk
            // (`selRecord.c:159-160`) — so the test never matches and every
            // `sel` argument reaches `recGblGetPrec`. Transcribed as C
            // behaves, not as it reads.
            _ => false,
        }
    }

    /// The field's type as the **dbd declares it**, which is the only type
    /// `recGblGetPrec` / `getMaxRangeValues` ever see.
    ///
    /// C reads `pdbFldDes->field_type` (`recGbl.c:127`, `:151`, `:169`) — the
    /// STATIC descriptor — so a `cvt_dbaddr` retype (the port's
    /// `runtime_typed`, DBF_NOACCESS in the dbd) never reaches the switch and
    /// the switch has no case for it. `None` reproduces that: no case, no
    /// write.
    fn static_field_type(&self, field: &str) -> Option<crate::types::DbFieldType> {
        let desc = self.field_desc(field)?;
        (!desc.runtime_typed).then_some(desc.dbf_type)
    }

    /// `recGblGetGraphicDouble` / `recGblGetControlDouble` for `field` — the
    /// same `getMaxRangeValues` table both C entry points share
    /// (`recGbl.c:146-171`). `None` where C's switch has no case (STRING,
    /// MENU, DEVICE, NOACCESS, links), which writes nothing.
    ///
    /// `declared_dbf` is C's `pdbFldDes->field_type` verbatim. Deciding
    /// menu-ness from `desc.menu` instead asked whether the field carries its
    /// own inline choice list, which `SCAN` and `DTYP` do not — their choices
    /// come from the scan table and the device registry — so both reported a
    /// `DBF_USHORT` range of 65535/0 to `gft` where C reports 0/0.
    fn rec_gbl_range_for(&self, field: &str) -> Option<(f64, f64)> {
        let desc = self.field_desc(field)?;
        crate::server::recgbl::rec_gbl_get_graphic_double(desc.declared_dbf)
    }

    /// Notify subscribers from a snapshot (call outside lock).
    /// Each entry carries its own posting mask: only subscribers whose
    /// mask intersects that field's mask are notified, and the delivered
    /// [`MonitorEvent`] reports that intersection — C
    /// `db_post_events(prec, &field, mask)` per-field granularity, then
    /// `pLog->mask = caEventMask & pevent->select` per subscriber.
    ///
    /// `backing` is the link metadata the process cycle resolved for this
    /// batch, at its own no-lock-held point. See [`Self::make_monitor_snapshot`]
    /// for why it has no default.
    pub fn notify_from_snapshot(&self, snapshot: &ProcessSnapshot, backing: LinkBacking<'_>) {
        use crate::server::database::filters::FilteredMonitorEvent;

        // Same ambient-origin inheritance as `notify_field_with_origin`:
        // a process cycle driven by an in-process writer's put tags its
        // posts with the writer's origin, so the writer's own filtered
        // subscriptions do not hear its cascade. 0 outside any scope.
        let origin = ambient_write_origin();

        for (field, value, posting_mask) in &snapshot.changed_fields {
            let posting_mask = *posting_mask;
            if let Some(subs) = self.subscribers.get(field) {
                // Build a full snapshot once per field (with display
                // metadata) and hand every subscriber a reference to that one
                // snapshot — C posts the fixed-size `db_field_log` and reads
                // the wide value by reference at delivery (`camessage.c:516`),
                // so a per-subscriber deep copy of an array value is a port
                // deviation, not parity.
                let mon_snap = Arc::new(self.make_monitor_snapshot(field, value.clone(), backing));
                for sub in subs {
                    // Paused subscriber (`db_event_disable`): suppress at
                    // the source — no delivery, no coalesce.
                    if !sub.active {
                        continue;
                    }
                    // Gate and narrow in one step through
                    // `Subscriber::delivered_mask`, which owns C's
                    // twice-used `caEventMask & pevent->select`. An empty
                    // posting mask means nothing changed and ands to zero,
                    // so it skips there rather than needing a check here.
                    if let Some(mask) = sub.delivered_mask(posting_mask) {
                        let event = MonitorEvent {
                            snapshot: mon_snap.clone(),
                            origin,
                            mask,
                        };
                        // Server-side filter chain (3.15.7). Empty chain
                        // is identity, so no behaviour change for the
                        // common no-filter case.
                        let filtered = if sub.filters.is_empty() {
                            Some(event)
                        } else {
                            sub.filters
                                .apply(FilteredMonitorEvent::new(event))
                                .map(|fe| fe.event)
                        };
                        let Some(event) = filtered else {
                            continue;
                        };
                        // C `db_queue_event_log`: append, or replace this
                        // monitor's last queued entry in place when the queue
                        // is in flow control or nearly full. The queue owns
                        // that decision and counts the displaced value.
                        sub.post(event);
                    }
                }
            }
        }
    }

    /// Notify subscribers of a specific field, filtering by event mask.
    ///
    /// The last wrapper that still answers for its callers: `none()` here is a
    /// claim that no caller of this function names a link-backed field, and it
    /// is made once for 25 production call sites rather than at each of them.
    /// [`Self::notify_field_backed`] is the form for a caller that cannot make
    /// that claim.
    pub fn notify_field(&mut self, field: &str, mask: crate::server::recgbl::EventMask) {
        self.notify_field_with_origin(field, mask, 0, LinkBacking::none());
    }

    /// [`Self::notify_field`] for a poster that may name a link-backed field
    /// and has resolved its backing.
    pub fn notify_field_backed(
        &mut self,
        field: &str,
        mask: crate::server::recgbl::EventMask,
        backing: LinkBacking<'_>,
    ) {
        self.notify_field_with_origin(field, mask, 0, backing);
    }

    /// C `db_post_events(precord, NULL, DBE_ALARM)`: post a record-wide
    /// alarm event. Delivers to every subscriber on any field whose mask
    /// includes DBE_ALARM, each carrying its own monitored field's current
    /// value (the per-field `notify_field` already filters by mask
    /// intersection). Used by the alarm-acknowledge (ACKT/ACKS) put path so
    /// an alarm-mask monitor on any field observes the acknowledgement.
    pub fn notify_record_alarm(&mut self, backing: LinkBacking<'_>) {
        // Every subscribed field, so a client monitoring a link-backed one
        // (`CALC.A`) is in the set — this poster takes a backing for that
        // reason and not because the alarm itself is link-backed.
        let fields: Vec<String> = self.subscribers.keys().cloned().collect();
        for field in fields {
            self.notify_field_backed(&field, crate::server::recgbl::EventMask::ALARM, backing);
        }
    }

    /// Notify subscribers with an origin tag for self-write filtering.
    ///
    /// This is C `db_post_events(precord, pfield, mask)` for one field, and —
    /// per the `last_posted` contract — the poster that advances the
    /// already-published value when `mask` carries a value class. Taking
    /// `&mut self` is what makes that unbypassable: there is no way to publish
    /// a field's value through the framework without the change detector
    /// learning that it was published.
    ///
    /// `backing` is the link metadata the put path resolved for this post, at
    /// its own no-lock-held point. See [`Self::make_monitor_snapshot`] for why
    /// it has no default.
    pub fn notify_field_with_origin(
        &mut self,
        field: &str,
        mask: crate::server::recgbl::EventMask,
        origin: u64,
        backing: LinkBacking<'_>,
    ) {
        use crate::server::database::filters::FilteredMonitorEvent;
        // A poster that carries no origin of its own inherits the ambient
        // one (0 outside any scope): this is how every post inside an
        // SNL writer's synchronous put+process cascade gets the writer's
        // tag without threading a parameter through the whole processing
        // machinery. An explicit origin always wins.
        let origin = if origin != 0 {
            origin
        } else {
            ambient_write_origin()
        };
        // A value-class post publishes the field to its DBE_VALUE/DBE_LOG
        // subscribers, exactly as C's `dbPut` does for the put field
        // (dbAccess.c:1414) — record it so the next process cycle's
        // change-detection loop does not publish the same value a second
        // time. An alarm-only / property-only post publishes no value, so it
        // leaves the map alone.
        let publishes_value = mask.intersects(
            crate::server::recgbl::EventMask::VALUE | crate::server::recgbl::EventMask::LOG,
        );
        let mut posted: Option<EpicsValue> = None;
        if let Some(subs) = self.subscribers.get(field) {
            if let Some(value) = self.resolve_field(field) {
                if publishes_value {
                    posted = Some(value.clone());
                }
                let mon_snap = Arc::new(self.make_monitor_snapshot(field, value, backing));
                for sub in subs {
                    // Paused subscriber (`db_event_disable`): suppress at
                    // the source — no delivery, no coalesce.
                    if !sub.active {
                        continue;
                    }
                    // Same single owner as the snapshot path: gate and
                    // narrow are one operation (C `dbEvent.c:896-900`).
                    if let Some(mask) = sub.delivered_mask(mask) {
                        let event = MonitorEvent {
                            snapshot: mon_snap.clone(),
                            origin,
                            mask,
                        };
                        // Server-side filter chain (3.15.7). Empty
                        // chain (the default for every subscriber
                        // until a `.{filter:opts}` PV-name suffix
                        // parser wires one in) is the identity, so
                        // existing subscribers see no behaviour
                        // change. A filter returning `None` silences
                        // this event for this subscriber only.
                        let filtered = if sub.filters.is_empty() {
                            Some(event)
                        } else {
                            sub.filters
                                .apply(FilteredMonitorEvent::new(event))
                                .map(|fe| fe.event)
                        };
                        let Some(event) = filtered else {
                            continue;
                        };
                        // Same single post owner as the snapshot path.
                        sub.post(event);
                    }
                }
            }
        }
        // The value is now published to this field's value-class subscribers:
        // hand it to the `last_posted` owner so the change detector does not
        // publish it again. Delivery to any individual subscriber may have
        // been filtered out, exactly as C's `db_post_events` may find an empty
        // `mlis` — C still leaves `monitor()`'s `*_lst` state advanced by the
        // cycle that ran, so the post, not the delivery, is what counts.
        if let Some(value) = posted {
            self.record_value_post(field, value);
        }
    }

    /// Add a subscriber for a specific field. Returns `None` when the
    /// per-field subscriber cap (`EPICS_CAS_MAX_SUBSCRIBERS_PER_PV`)
    /// is reached. the parallel cap on `ProcessVariable`
    /// defends against a misbehaving client opening many
    /// MONITOR ops against one shared PV; the same defence is needed
    /// for record fields, which the CA server's
    /// `ChannelTarget::RecordField` path lands on.
    pub fn add_subscriber(
        &mut self,
        field: &str,
        sid: u32,
        data_type: DbFieldType,
        mask: u16,
    ) -> Option<EventReader> {
        self.add_subscriber_on(&EventUser::new(), field, sid, data_type, mask)
    }

    /// Add a field subscriber whose events queue on `user`'s event queue —
    /// C `db_add_event` with the circuit's `event_user` as context. Every
    /// subscription on one CA circuit shares that queue and therefore its
    /// `nDuplicates`, so a duplicate queued for one of them releases the
    /// EVENTS_OFF drain for all of them (`dbEvent.c:947`). In-process consumers
    /// use [`Self::add_subscriber`], which gives each its own `event_user`.
    pub fn add_subscriber_on(
        &mut self,
        user: &EventUser,
        field: &str,
        sid: u32,
        data_type: DbFieldType,
        mask: u16,
    ) -> Option<EventReader> {
        let cap = crate::server::pv::max_subscribers_per_pv();
        // A destroyed record takes no new monitor, so `destroyed => no
        // subscribers` survives a CREATE_CHAN + EVENT_ADD that races the
        // removal. Both are `&mut self`, so there is no window between them.
        if self.destroyed {
            return None;
        }
        let field_str = field.to_string();
        let bucket = self.subscribers.entry(field_str.clone()).or_default();
        // Reap rows whose consumer is gone before
        // counting against the cap. A record field whose value
        // never changes (e.g. a quasi-static catalog field) never
        // triggers `notify_field_with_origin`'s retain-filter, so
        // a long-lived subscribe-disconnect storm could pin the
        // bucket at `cap` worth of dead rows and lock out
        // genuine new subscribers.
        bucket.retain(|s| !s.is_closed());
        if bucket.len() >= cap {
            tracing::warn!(
                record = %self.name,
                field = %field_str,
                live = bucket.len(),
                cap,
                "record field subscriber cap reached, refusing add_subscriber"
            );
            return None;
        }
        let (sink, reader) = crate::server::event_queue::attach(user, sid);
        bucket.push(Subscriber {
            sid,
            data_type,
            mask,
            sink,
            filters: crate::server::database::filters::FilterChain::new(),
            active: true,
        });
        // Initialize last_posted with current value so the first process cycle
        // doesn't treat it as "changed" (the initial value is already sent
        // to the client as part of EVENT_ADD response).
        if !self.last_posted.contains_key(&field_str) {
            if let Some(val) = self.resolve_field(&field_str) {
                self.last_posted.insert(field_str, val);
            }
        }
        Some(reader)
    }

    /// Attach a filter to the most recently added subscriber for
    /// `field`. Returns `false` when no subscriber exists yet on that
    /// field (call `add_subscriber` first). The CA / PVA channel-name
    /// parsers will use this once `.{filter:opts}` syntax is wired.
    /// Tests can also use it directly to compose filter chains.
    pub fn attach_filter_to_last_subscriber(
        &mut self,
        field: &str,
        filter: std::sync::Arc<dyn crate::server::database::filters::SubscriptionFilter>,
    ) -> bool {
        if let Some(bucket) = self.subscribers.get_mut(field) {
            if let Some(sub) = bucket.last_mut() {
                sub.filters.push(filter);
                return true;
            }
        }
        false
    }

    /// Remove a subscriber by subscription ID from all fields.
    pub fn remove_subscriber(&mut self, sid: u32) {
        for subs in self.subscribers.values_mut() {
            subs.retain(|s| s.sid != sid);
        }
    }

    /// Destroy this record: drop every field monitor and refuse every future
    /// one. The record-backed half of the rule
    /// [`crate::server::pv::ProcessVariable::destroy`] states for simple PVs,
    /// so one sweep in a server closes both kinds of channel. Returns `true`
    /// for the call that performed the transition.
    pub(crate) fn destroy(&mut self) -> bool {
        let first = !self.destroyed;
        self.destroyed = true;
        self.subscribers.clear();
        first
    }

    /// Whether `Self::destroy` has run.
    pub fn is_destroyed(&self) -> bool {
        self.destroyed
    }

    /// Pause / resume one subscriber's event flow at the source
    /// (`db_event_disable` / `db_event_enable`). `active == false`
    /// suppresses every subsequent post to this subscriber, so the record stops
    /// doing per-event work for it. Entries already queued stay queued and are
    /// still delivered, exactly as in C: `db_event_disable` only unlinks the
    /// subscription from the record's monitor list (`dbEvent.c:524-535`) and
    /// never reaches into the event queue. No-op if no subscriber has this
    /// `sid`. The caller holds the record write lock, so this is exclusive with
    /// the read-locked post paths that consult `Subscriber::active`.
    pub fn set_subscriber_active(&mut self, sid: u32, active: bool) {
        for subs in self.subscribers.values_mut() {
            for sub in subs.iter_mut() {
                if sub.sid == sid {
                    sub.active = active;
                }
            }
        }
    }

    /// Clean up subscriber rows whose consumer is gone.
    pub fn cleanup_subscribers(&mut self) {
        for subs in self.subscribers.values_mut() {
            subs.retain(|s| !s.is_closed());
        }
    }
}

/// C `recGblCheckDeadband` (recGbl.c:345-370), spelled as C spells it:
///
/// ```c
/// double delta = 0;
/// if (finite(newval) && finite(*poldval)) {
///     delta = *poldval - newval;
///     if (delta < 0.0) delta = -delta;
/// }
/// else if (!isnan(newval) != !isnan(*poldval) ||
///          !isinf(newval) != !isinf(*poldval)) delta = epicsINF;
/// else if (isinf(newval) && newval != *poldval) delta = epicsINF;
/// if (delta > deadband) { *monitor_mask |= add_mask; *poldval = newval; }
/// ```
///
/// The `delta = 0` initialiser is load-bearing: the pairs no branch matches
/// — both NaN, or two same-signed infinities — reach `0 > deadband`, so they
/// fire only for a negative deadband and otherwise leave `*poldval` alone.
/// That is why the whole rule has to stay a single `delta > deadband` rather
/// than a chain of early returns, and why NaN cannot be read as a marker
/// here: C compares a NaN `*poldval` against a NaN `newval` by these rules
/// and finds them unchanged. `dbnd.c parse_ok` relies on exactly that when it
/// seeds its own `last` to `epicsNAN`.
///
/// `oldval` is `None` for the record types that carry no MLST/ALST cell to
/// hold a last-posted value; nothing was posted, so the first post is
/// unconditional. C has no such state — its MLST is a plain double the record
/// type initialises (0 for `calc` and `sel`, `prec->val` for `ai`,
/// aiRecord.c:129-130).
pub(crate) fn check_deadband(newval: f64, oldval: Option<f64>, deadband: f64) -> bool {
    let Some(oldval) = oldval else {
        return true;
    };
    let delta = if newval.is_finite() && oldval.is_finite() {
        (oldval - newval).abs()
    } else if newval.is_nan() != oldval.is_nan() || newval.is_infinite() != oldval.is_infinite() {
        // One is NaN or +/-inf and the other is not.
        f64::INFINITY
    } else if newval.is_infinite() && newval != oldval {
        // One is +inf, the other -inf.
        f64::INFINITY
    } else {
        0.0
    };
    delta > deadband
}

#[cfg(test)]
mod device_menu_marking_tests {
    use super::*;
    use crate::server::records::ai::AiRecord;
    use crate::server::records::calc::CalcRecord;
    use crate::server::records::mbbo::MbboRecord;

    /// C `dbAccess.c:176-179`: a `DBF_DEVICE` field whose record type declares
    /// no device support has `pfldDes->ftPvt == NULL` and takes `goto nostrs`,
    /// which clears `DBR_ENUM_STRS` — the client is sent NO choice list.
    ///
    /// `calc` declares no `device()` line, so QSRV2 omits `value.choices` on
    /// `CALC.DTYP`. The port used to default the missing menu to `[]` and mark
    /// an empty list instead.
    #[test]
    fn dtyp_of_a_record_type_with_no_device_support_supplies_no_choices() {
        let inst = RecordInstance::new("X".into(), CalcRecord::default());
        assert!(
            super::super::dbd_generated::device_menu("calc").is_none(),
            "precondition: calc declares no device() line (C ftPvt == NULL)"
        );
        assert!(
            inst.device_choices().is_none(),
            "a record type with no device menu must report None, not an empty list"
        );
        assert!(
            inst.enum_string_form_for("DTYP").is_none(),
            "DTYP must supply no enum-string form, so no `value.choices` is marked"
        );
    }

    /// The other side of C's `dbAccess.c:205` comment — *"indicate option data
    /// not available. distinct from no_str==0"*. `ai` DOES declare device
    /// support, so its menu exists and its choices are served.
    #[test]
    fn dtyp_of_a_record_type_with_device_support_supplies_its_choices() {
        let inst = RecordInstance::new("X".into(), AiRecord::default());
        let choices = inst
            .device_choices()
            .expect("ai declares device() lines, so its menu exists");
        assert!(
            choices.iter().any(|c| c.as_str_lossy() == "Soft Channel"),
            "ai's device menu must carry its declared choices, got {choices:?}"
        );
        assert!(inst.enum_string_form_for("DTYP").is_some());
    }

    /// An unset `DTYP` is index 0 on both sides of the distinction — a record
    /// type with no device menu has no slot for any DTYP, so the index stays 0
    /// rather than panicking or shifting.
    #[test]
    fn dtyp_index_is_zero_when_the_record_type_has_no_device_menu() {
        let inst = RecordInstance::new("X".into(), CalcRecord::default());
        assert_eq!(inst.dtyp_index(), 0);
    }

    /// A downstream crate's registered device menu (asyn's) is merged AFTER the
    /// base-declared choices, matching a C fat softIoc that loaded `asyn.dbd`:
    /// `mbbo.DTYP` = the three base soft entries then `asynInt32`,
    /// `asynUInt32Digital`, in that order. `dtyp_index` reads the merged list,
    /// so an `mbbo` bound to `asynInt32` reports index 3 — the wire value C
    /// serves — instead of the appended-as-own-slot index the port gave before
    /// the menu was known.
    #[test]
    fn a_registered_device_menu_merges_after_the_base_declared_choices() {
        // The list asyn's generated `dbd_generated::DEVICE_MENU_MBBO` carries.
        static ASYN_MBBO: &[&str] = &["asynInt32", "asynUInt32Digital"];
        super::super::register_device_menu("mbbo", ASYN_MBBO);

        let mut inst = RecordInstance::new("X".into(), MbboRecord::default());
        let merged: Vec<String> = inst
            .device_choices()
            .expect("mbbo declares device() lines")
            .iter()
            .map(|c| c.as_str_lossy().into_owned())
            .collect();
        assert_eq!(
            merged,
            vec![
                "Soft Channel",
                "Raw Soft Channel",
                "Async Soft Channel",
                "asynInt32",
                "asynUInt32Digital",
            ],
            "base-declared choices first, asyn-contributed appended in asyn.dbd order"
        );

        inst.common.dtyp = "asynInt32".into();
        assert_eq!(
            inst.dtyp_index(),
            3,
            "an asyn DTYP indexes into the merged menu, not an appended own slot"
        );
    }

    /// The None-vs-empty contract survives the merge: a record type neither
    /// base nor any downstream crate contributes a `device()` for (calc) stays
    /// `None`, never `Some([])`, even after asyn menus are registered in this
    /// process.
    #[test]
    fn calc_stays_none_after_asyn_menus_are_registered() {
        static ASYN_MBBO: &[&str] = &["asynInt32", "asynUInt32Digital"];
        super::super::register_device_menu("mbbo", ASYN_MBBO);

        let inst = RecordInstance::new("X".into(), CalcRecord::default());
        assert!(
            inst.device_choices().is_none(),
            "calc declares no device() and gets no contribution — still None"
        );
    }
}

#[cfg(test)]
mod property_support_owner_tests {
    use crate::server::record::record_trait::default_property_support;
    use crate::server::snapshot::PropertySupport as P;

    /// `sseqRecord.c:124-144` — the rset table NULLs every property slot
    /// except `get_precision`:
    ///
    /// ```c
    /// NULL,           /* get_units */
    /// get_precision,  /* get_precision */
    /// NULL,           /* get_enum_str */
    /// NULL,           /* get_enum_strs */
    /// NULL,           /* put_enum_str */
    /// NULL,           /* get_graphic_double */
    /// NULL,           /* get_control_double */
    /// NULL            /* get_alarm_double */
    /// ```
    ///
    /// `sseq` was previously grouped with the full-numeric synApps types, so
    /// the port marked six leaves per field that QSRV2 omits entirely.
    #[test]
    fn sseq_supplies_only_precision() {
        assert_eq!(
            default_property_support("sseq"),
            P {
                precision: true,
                ..P::NONE
            }
        );
    }

    /// A record type the table does not name keeps the permissive
    /// `NUMERIC` default rather than silently losing metadata. This is the
    /// arm `asyn` used to land on — and why marking had to become a trait
    /// method: asyn-rs cannot add a row here.
    #[test]
    fn an_untranscribed_record_type_keeps_the_permissive_default() {
        assert_eq!(default_property_support("no-such-record-type"), P::NUMERIC);
    }
}

#[cfg(test)]
mod metadata_cache_tests {
    use super::*;
    use crate::server::records::ai::AiRecord;

    /// Helper: build an AiRecord wrapped in a RecordInstance with EGU/PREC/HOPR/LOPR set.
    fn ai_instance() -> RecordInstance {
        let mut rec = AiRecord::default();
        let _ = rec.put_field("EGU", EpicsValue::String("degC".into()));
        let _ = rec.put_field("PREC", EpicsValue::Short(2));
        let _ = rec.put_field("HOPR", EpicsValue::Double(100.0));
        let _ = rec.put_field("LOPR", EpicsValue::Double(0.0));
        let _ = rec.put_field("VAL", EpicsValue::Double(25.0));
        RecordInstance::new("TEMP".to_string(), rec)
    }

    /// a record-field monitor whose event queue has run short of room
    /// replaces its last queued entry in place (C `db_queue_event_log`,
    /// `dbEvent.c:812-827`), and the displaced value — which the consumer never
    /// observed — must be counted in the shared `dropped_monitor_events()`
    /// counter (C `nreplace`), the same accounting a `ProcessVariable` post
    /// uses. Before the fix the record-field path overwrote its coalesce slot
    /// without counting, hiding slow-consumer loss on the path most CA/PVA
    /// database monitors use. The counter is process-global, so the assertion is
    /// a strict monotonic increase (robust under parallel tests); the
    /// revert-verify runs this test in isolation.
    #[test]
    fn bfr10_record_field_overflow_counts_dropped_event() {
        use crate::server::event_queue::{event_que_size, events_per_que};
        use crate::server::pv::dropped_monitor_events;
        use crate::server::recgbl::EventMask;
        let mut inst = ai_instance();
        // Keep the reader alive and do NOT drain, so the ring fills to the
        // replace threshold and later posts displace the tail entry.
        let _reader = inst
            .add_subscriber(
                "VAL",
                1,
                crate::types::DbFieldType::Double,
                EventMask::VALUE.bits(),
            )
            .expect("subscriber added");
        let before = dropped_monitor_events();
        let posts = event_que_size() - events_per_que() + 10;
        for _ in 0..posts {
            inst.notify_field_with_origin("VAL", EventMask::VALUE, 0, LinkBacking::none());
        }
        let after = dropped_monitor_events();
        assert!(
            after > before,
            "a post that replaces an unobserved queued entry must record a \
             dropped monitor event (before={before}, after={after})"
        );
    }

    #[test]
    fn metadata_cache_source_set_check() {
        // Every field `populate_display_info` / `populate_control_info` /
        // `populate_enum_info` reads.
        assert!(is_metadata_cache_source("EGU"));
        assert!(is_metadata_cache_source("PREC"));
        assert!(is_metadata_cache_source("HOPR"));
        assert!(is_metadata_cache_source("LOPR"));
        assert!(is_metadata_cache_source("DRVH"));
        assert!(is_metadata_cache_source("ZNAM"));
        assert!(is_metadata_cache_source("ZRST"));
        assert!(is_metadata_cache_source("FFST"));

        // The cache holds no alarm limits — `explicit_alarm_limits` is on the
        // live `apply_field_metadata_override` path — so HIHI is property-class
        // without being a cache source.
        assert!(!is_metadata_cache_source("HIHI"));
        assert!(!is_metadata_cache_source("VAL"));
        assert!(!is_metadata_cache_source("DESC"));
        assert!(!is_metadata_cache_source("SCAN"));
        assert!(!is_metadata_cache_source("PHAS"));
    }

    #[test]
    fn cache_starts_empty_then_populates_on_first_snapshot() {
        let inst = ai_instance();

        // Cache starts empty
        assert!(inst.metadata_cache.lock().unwrap().is_none());

        // First snapshot triggers populate + cache store
        let snap = inst.snapshot_for_field("VAL").unwrap();
        let display = snap.display.expect("ai snapshot must have display");
        assert_eq!(display.units, "degC");
        assert_eq!(display.precision, 2);
        assert_eq!(display.upper_disp_limit, 100.0);
        assert_eq!(display.lower_disp_limit, 0.0);

        // Cache is now populated
        assert!(inst.metadata_cache.lock().unwrap().is_some());
    }

    #[test]
    fn q_form_info_tag_sets_display_form_index() {
        // pvxs maps the `Q:form` info tag to `display.form.index` for the
        // VAL field (iocsource.cpp:42-62). "Hex" is slot 4 of the
        // seven-entry menu (Default/String/Binary/Decimal/Hex/...).
        let mut inst = ai_instance();
        inst.set_info("Q:form", "Hex");
        let snap = inst.snapshot_for_field("VAL").unwrap();
        let display = snap.display.expect("ai snapshot must have display");
        assert_eq!(display.form, 4, "Q:form=Hex -> display.form index 4");
    }

    /// R16-31: `Q:form` is a record-level info tag, but QSRV assigns
    /// `display.form.index` only when the channel addresses the VAL field
    /// (`if(dbIsValueField(dbChannelFldDes(chan)))`, `iocsource.cpp:53`). A
    /// snapshot of any other field of the same record reports the default
    /// form, on both the GET and the monitor producer.
    #[test]
    fn q_form_applies_to_the_val_field_only() {
        let mut inst = ai_instance();
        inst.set_info("Q:form", "Hex");

        let val = inst.snapshot_for_field("VAL").unwrap();
        assert_eq!(val.display.expect("ai display").form, 4);

        for non_val in ["RVAL", "SEVR", "HOPR"] {
            let Some(snap) = inst.snapshot_for_field(non_val) else {
                panic!("ai.{non_val} must resolve");
            };
            assert_eq!(
                snap.display.expect("ai display").form,
                0,
                "Q:form must not reach ai.{non_val} — pvxs applies it to VAL only"
            );
        }

        // The monitor producer shares the same per-field owner.
        let update = inst.make_monitor_snapshot("RVAL", EpicsValue::Long(7), LinkBacking::none());
        assert_eq!(
            update.display.expect("ai display").form,
            0,
            "a monitor update on a non-VAL field carries the default form too"
        );
        let update =
            inst.make_monitor_snapshot("VAL", EpicsValue::Double(1.0), LinkBacking::none());
        assert_eq!(update.display.expect("ai display").form, 4);
    }

    #[test]
    fn q_form_absent_or_unknown_leaves_form_default() {
        // No `Q:form` tag -> form stays 0 (Default).
        let inst = ai_instance();
        let snap = inst.snapshot_for_field("VAL").unwrap();
        assert_eq!(snap.display.expect("ai display").form, 0);

        // Unrecognised tag -> pvxs leaves the index untouched (0).
        let mut inst2 = ai_instance();
        inst2.set_info("Q:form", "Nonsense");
        let snap2 = inst2.snapshot_for_field("VAL").unwrap();
        assert_eq!(snap2.display.expect("ai display").form, 0);
    }

    /// `info(Q:time:tag)` resolves to pvxs's `nsecMask`
    /// (`ioc/typeutils.cpp:79-88`). The prefix test there is a byte-exact
    /// `strncmp("nsec:lsb:", 9)` and the digit count is fed straight to
    /// `(uint64_t(1u)<<dig)-1u` — no case folding, no whitespace tolerance
    /// around the prefix, and no bounds clamp. Each boundary gets a case.
    #[test]
    fn qtime_nsec_mask_matches_pvxs_updatensecmask() {
        let cases: &[(&str, u64)] = &[
            // parses: `epicsParseInt32` skips whitespace around the digits
            // and accepts a sign.
            ("nsec:lsb:20", (1 << 20) - 1),
            ("nsec:lsb:1", 1),
            ("nsec:lsb: 4 ", 0xF),
            ("nsec:lsb:+4", 0xF),
            // no clamp: 31 is the mask pvxs actually serves (the old Rust
            // `(1..=30)` guard dropped it), and 0 is pvxs's "off" mask.
            ("nsec:lsb:31", 0x7FFF_FFFF),
            ("nsec:lsb:0", 0),
            // `strncmp` is byte-exact: case-folded or whitespace-split
            // prefixes do not match, so pvxs leaves `nsecMask` at 0.
            ("NSEC:LSB:4", 0),
            ("Nsec:Lsb:4", 0),
            ("nsec: lsb: 4", 0),
            (" nsec:lsb:4", 0),
            // `epicsParseInt32` failures: no conversion, extraneous trailing
            // bytes, overflow past epicsInt32.
            ("nsec:lsb:", 0),
            ("nsec:lsb:abc", 0),
            ("nsec:lsb:4x", 0),
            ("nsec:lsb:4 5", 0),
            ("nsec:lsb:99999999999999999999", 0),
            ("nsec:lsb:2147483648", 0),
        ];
        for (tag, want) in cases {
            let mut inst = ai_instance();
            inst.set_info("Q:time:tag", *tag);
            assert_eq!(
                inst.qtime_nsec_mask(),
                *want,
                "info(Q:time:tag, {tag:?}) must resolve to nsecMask {want:#x}"
            );
        }
        // Tag absent entirely → pvxs never enters the `if(auto val = ...)`
        // body and `nsecMask` stays 0.
        assert_eq!(ai_instance().qtime_nsec_mask(), 0);
    }

    /// End-to-end on the snapshot: `nsec:lsb:31` publishes
    /// `nanoseconds & ~mask` (0, since nanoseconds < 1e9 < 2^31) and
    /// `userTag = nanoseconds & mask` (pvxs `iocsource.cpp:239-248`). The
    /// old `(1..=30)` clamp served the raw nanoseconds and the record's
    /// utag instead.
    #[test]
    fn qtime_nsec_lsb_31_is_served_not_ignored() {
        use std::time::{Duration, SystemTime};
        let mut inst = ai_instance();
        // 123_456_700, not …789: Windows `SystemTime` is a FILETIME with 100 ns
        // resolution, so a sub-100 ns literal is truncated on readback and the
        // assertion below would see …700. Any value < 2^31 exercises the
        // nsec:lsb:31 mask identically, so pin one that survives the round trip.
        inst.common.time = SystemTime::UNIX_EPOCH + Duration::new(42, 123_456_700);
        inst.common.utag = 5;
        inst.set_info("Q:time:tag", "nsec:lsb:31");

        let snap = inst.snapshot_for_field("VAL").unwrap();
        assert_eq!(snap.user_tag, 123_456_700);
        assert_eq!(snap.timestamp.subsec_nanos(), 0);
        assert_eq!(snap.timestamp.unix_secs(), 42);
    }

    /// The monitor path applies the same `Q:time:tag` nsec split as GET.
    /// Pre-fix, `make_monitor_snapshot` skipped `apply_nsec_mask`, so a
    /// monitor update on a `nsec:lsb:N` record posted the raw nanoseconds
    /// and the record utag while a GET of the same channel served the
    /// split — upstream pvxs PR #189 is the same defect in its
    /// `subscriptionCallback`.
    #[test]
    fn qtime_nsec_mask_applies_on_the_monitor_path() {
        use std::time::{Duration, SystemTime};
        let mut inst = ai_instance();
        // 100 ns-multiple so the subsec_nanos assertion holds on Windows too;
        // see qtime_nsec_lsb_31_is_served_not_ignored for the FILETIME reason.
        inst.common.time = SystemTime::UNIX_EPOCH + Duration::new(42, 123_456_700);
        inst.common.utag = 5;
        inst.set_info("Q:time:tag", "nsec:lsb:31");

        let mon = inst.make_monitor_snapshot("VAL", EpicsValue::Double(1.0), LinkBacking::none());
        assert_eq!(mon.user_tag, 123_456_700);
        assert_eq!(mon.timestamp.subsec_nanos(), 0);
        assert_eq!(mon.timestamp.unix_secs(), 42);
    }

    /// The mirror boundary: a tag pvxs's `strncmp` rejects must leave the
    /// timestamp and the record's own utag alone. The old case-insensitive
    /// split matched `NSEC:LSB:4` and masked the wire timestamp pvxs serves
    /// unmasked.
    #[test]
    fn qtime_uppercase_tag_leaves_timestamp_untouched() {
        use std::time::{Duration, SystemTime};
        let mut inst = ai_instance();
        // 100 ns-multiple so the subsec_nanos assertion holds on Windows too;
        // see qtime_nsec_lsb_31_is_served_not_ignored for the FILETIME reason.
        inst.common.time = SystemTime::UNIX_EPOCH + Duration::new(42, 123_456_700);
        inst.common.utag = 5;
        inst.set_info("Q:time:tag", "NSEC:LSB:4");

        let snap = inst.snapshot_for_field("VAL").unwrap();
        assert_eq!(
            snap.user_tag, 5,
            "record utag must survive a non-matching tag"
        );
        assert_eq!(snap.timestamp.subsec_nanos(), 123_456_700);
    }

    /// the served `timeStamp.userTag` defaults to the record's `utag`
    /// (pvxs `iocsource.cpp:245`), on both the GET (`snapshot_for_field`)
    /// and MONITOR (`make_monitor_snapshot`) paths. Pre-fix both hard-set
    /// it to 0, dropping the record's tag. A bit-31 utag also pins the
    /// `u64 -> i32` narrowing: the low 32 bits' pattern is preserved
    /// (no clamp), matching pvxs assigning `epicsUTag` into the `Int32`
    /// wire field.
    #[test]
    fn snapshot_serves_record_utag_as_timestamp_usertag() {
        let mut inst = ai_instance();
        // no `info(Q:time:tag, ...)` on this record, so the nsec-LSB
        // override never fires and the utag default is what is served.
        inst.common.utag = 0x9000_0000;
        let want = 0x9000_0000u32 as i32;

        let get = inst.snapshot_for_field("VAL").unwrap();
        assert_eq!(
            get.user_tag, want,
            "GET path must serve the record's utag as timeStamp.userTag"
        );

        let mon = inst.make_monitor_snapshot("VAL", EpicsValue::Double(1.0), LinkBacking::none());
        assert_eq!(
            mon.user_tag, want,
            "MONITOR path must carry the record's utag too"
        );
    }

    #[test]
    fn cache_hit_returns_same_metadata() {
        let inst = ai_instance();

        // Prime the cache
        let snap1 = inst.snapshot_for_field("VAL").unwrap();
        let display1 = snap1.display.unwrap();

        // Subsequent snapshots return the same cached metadata
        let snap2 = inst.snapshot_for_field("VAL").unwrap();
        let display2 = snap2.display.unwrap();

        assert_eq!(display1.units, display2.units);
        assert_eq!(display1.precision, display2.precision);
        assert_eq!(display1.upper_disp_limit, display2.upper_disp_limit);
        assert_eq!(display1.lower_disp_limit, display2.lower_disp_limit);
    }

    #[test]
    fn invalidate_clears_cache() {
        let inst = ai_instance();
        let _ = inst.snapshot_for_field("VAL");
        assert!(inst.metadata_cache.lock().unwrap().is_some());

        inst.invalidate_metadata_cache();
        assert!(inst.metadata_cache.lock().unwrap().is_none());
    }

    #[test]
    fn notify_field_written_invalidates_for_metadata_field() {
        let inst = ai_instance();
        let _ = inst.snapshot_for_field("VAL");
        assert!(inst.metadata_cache.lock().unwrap().is_some());

        // Writing a metadata field should invalidate
        inst.notify_field_written("EGU");
        assert!(inst.metadata_cache.lock().unwrap().is_none());
    }

    #[test]
    fn notify_field_written_skips_non_metadata_field() {
        let inst = ai_instance();
        let _ = inst.snapshot_for_field("VAL");
        assert!(inst.metadata_cache.lock().unwrap().is_some());

        // Writing a value field should NOT invalidate the cache
        inst.notify_field_written("VAL");
        assert!(inst.metadata_cache.lock().unwrap().is_some());

        // DESC is not property-class either — its cache invalidation
        // is owned by the DESC arm of `put_common_field`, not by this
        // notify path (UI-106).
        inst.notify_field_written("DESC");
        assert!(inst.metadata_cache.lock().unwrap().is_some());
    }

    #[test]
    fn notify_field_written_is_case_insensitive() {
        let inst = ai_instance();
        let _ = inst.snapshot_for_field("VAL");
        assert!(inst.metadata_cache.lock().unwrap().is_some());

        // Lowercase metadata field name should still trigger invalidation
        inst.notify_field_written("egu");
        assert!(inst.metadata_cache.lock().unwrap().is_none());
    }

    /// epics-base faac1df1 — `notify_field_written_if_changed` must
    /// SKIP the cache invalidation when the metadata field's value
    /// didn't actually change. Otherwise a stream of idempotent puts
    /// from a CSS panel binds DBE_PROPERTY subscribers to bogus
    /// "property changed" events on every cycle.
    #[test]
    fn notify_field_written_if_changed_skips_when_unchanged() {
        let mut inst = ai_instance();
        let _ = inst.snapshot_for_field("VAL");
        assert!(inst.metadata_cache.lock().unwrap().is_some());

        // Capture prev, do a no-op put, then notify — cache must remain.
        let prev = inst.record.get_field("EGU");
        let _ = inst.record.put_field("EGU", prev.clone().unwrap());
        inst.notify_field_written_if_changed("EGU", prev.as_ref(), LinkBacking::none());
        assert!(
            inst.metadata_cache.lock().unwrap().is_some(),
            "no-op put must not invalidate the metadata cache"
        );
    }

    /// And when the value DID change, the cache must invalidate.
    #[test]
    fn notify_field_written_if_changed_invalidates_on_real_change() {
        let mut inst = ai_instance();
        let _ = inst.snapshot_for_field("VAL");
        assert!(inst.metadata_cache.lock().unwrap().is_some());

        let prev = inst.record.get_field("EGU");
        let _ = inst
            .record
            .put_field("EGU", EpicsValue::String("kPa".into()));
        inst.notify_field_written_if_changed("EGU", prev.as_ref(), LinkBacking::none());
        assert!(
            inst.metadata_cache.lock().unwrap().is_none(),
            "real metadata change must invalidate cache"
        );
    }

    /// UI-106 / epics-base#785 — DESC feeds `display.description`
    /// (pvxs fills it on every metadata populate, iocsource.cpp:306-310),
    /// and a changed DESC refreshes the cache at its write owner so the
    /// next snapshot serves the new text.
    #[test]
    fn desc_reaches_display_description_and_a_write_refreshes_it() {
        let mut inst = ai_instance();
        inst.put_common_field("DESC", EpicsValue::String("before".into()))
            .unwrap();
        let snap = inst.snapshot_for_field("VAL").unwrap();
        assert_eq!(
            snap.display.as_ref().unwrap().description.as_str_lossy(),
            "before"
        );
        inst.put_common_field("DESC", EpicsValue::String("after".into()))
            .unwrap();
        assert!(
            inst.metadata_cache.lock().unwrap().is_none(),
            "a changed DESC must invalidate the metadata cache"
        );
        let snap = inst.snapshot_for_field("VAL").unwrap();
        assert_eq!(
            snap.display.as_ref().unwrap().description.as_str_lossy(),
            "after"
        );
    }

    /// …and an idempotent DESC put must NOT invalidate — same
    /// discipline as faac1df1 for the property-class fields.
    #[test]
    fn an_idempotent_desc_put_keeps_the_cache() {
        let mut inst = ai_instance();
        inst.put_common_field("DESC", EpicsValue::String("same".into()))
            .unwrap();
        let _ = inst.snapshot_for_field("VAL");
        assert!(inst.metadata_cache.lock().unwrap().is_some());
        inst.put_common_field("DESC", EpicsValue::String("same".into()))
            .unwrap();
        assert!(
            inst.metadata_cache.lock().unwrap().is_some(),
            "an unchanged DESC must not invalidate the metadata cache"
        );
    }

    /// Non-metadata fields don't carry property semantics — the
    /// `if_changed` variant must never invalidate for them, matching
    /// the existing `notify_field_written` short-circuit.
    #[test]
    fn notify_field_written_if_changed_skips_non_metadata_field() {
        let mut inst = ai_instance();
        let _ = inst.snapshot_for_field("VAL");
        assert!(inst.metadata_cache.lock().unwrap().is_some());
        // VAL is neither a cache source nor `prop(YES)` — must be skipped
        // even with a changed value.
        inst.notify_field_written_if_changed("VAL", None, LinkBacking::none());
        assert!(inst.metadata_cache.lock().unwrap().is_some());
    }

    #[test]
    fn cache_picks_up_new_value_after_invalidation() {
        let mut inst = ai_instance();

        // First snapshot: degC
        let snap1 = inst.snapshot_for_field("VAL").unwrap();
        assert_eq!(snap1.display.unwrap().units, "degC");

        // Mutate EGU and invalidate
        let _ = inst
            .record
            .put_field("EGU", EpicsValue::String("mV".into()));
        inst.notify_field_written("EGU");

        // Second snapshot: mV (rebuilt)
        let snap2 = inst.snapshot_for_field("VAL").unwrap();
        assert_eq!(snap2.display.unwrap().units, "mV");
    }

    /// R19-41: every snapshot carries the mask of which properties the
    /// channel SUPPLIES — C's `rset` slots (`dbAccess.c:336-427` clears the
    /// option bit of each NULL slot) narrowed to the addressed field. One
    /// case per gate boundary; the three record types are the ones measured
    /// against pvxs, which marks none of these leaves.
    #[test]
    fn property_support_masks_what_the_record_type_does_not_supply() {
        use crate::server::records::longout::LongoutRecord;
        use crate::server::records::stringout::StringoutRecord;
        use crate::server::records::waveform::WaveformRecord;

        // ai VAL (DBF_DOUBLE): every numeric slot, no enum strings.
        let ai = ai_instance();
        let p = ai.snapshot_for_field("VAL").unwrap().properties;
        assert_eq!(p, PropertySupport::NUMERIC);
        assert_eq!(
            ai.snapshot_for_field("VAL").unwrap().precision(),
            Some(2),
            "an ai supplies get_precision and VAL is DBF_DOUBLE"
        );

        // ai RVAL (DBF_LONG): the SAME rset, but C keeps DBR_PRECISION only
        // for DBF_FLOAT/DBF_DOUBLE (`dbAccess.c:386-395`).
        let rval = ai.snapshot_for_field("RVAL").unwrap();
        assert!(
            !rval.properties.precision && rval.precision().is_none(),
            "a non-float field supplies no precision even when the rset does"
        );
        assert!(
            rval.properties.units,
            "the other slots are unaffected by the field's type"
        );

        // longout: `#define get_precision NULL`.
        let lo = RecordInstance::new("LO".to_string(), LongoutRecord::default());
        let lo = lo.snapshot_for_field("VAL").unwrap();
        assert!(!lo.properties.precision && lo.precision().is_none());
        assert!(lo.properties.units && lo.properties.graphic_double);

        // stringout: no property slot at all.
        let so = RecordInstance::new("SO".to_string(), StringoutRecord::default());
        let so = so.snapshot_for_field("VAL").unwrap();
        assert_eq!(so.properties, PropertySupport::NONE);
        assert!(so.units().is_none(), "a stringout supplies no EGU");

        // waveform: `#define get_alarm_double NULL`.
        let wf = RecordInstance::new("WF".to_string(), WaveformRecord::default());
        let wf = wf.snapshot_for_field("VAL").unwrap();
        assert!(
            !wf.properties.alarm_double && wf.alarm_limits().is_none(),
            "a waveform supplies no alarm limits — a GUI must not draw bands at zero"
        );
        assert!(wf.properties.units && wf.properties.graphic_double);
    }

    #[test]
    fn make_monitor_snapshot_uses_cache() {
        let inst = ai_instance();
        assert!(inst.metadata_cache.lock().unwrap().is_none());

        // make_monitor_snapshot should also populate the cache
        let snap = inst.make_monitor_snapshot("VAL", EpicsValue::Double(42.0), LinkBacking::none());
        assert!(snap.display.is_some());
        assert!(inst.metadata_cache.lock().unwrap().is_some());

        // Subsequent call hits cache
        let snap2 =
            inst.make_monitor_snapshot("VAL", EpicsValue::Double(43.0), LinkBacking::none());
        let d1 = snap.display.unwrap();
        let d2 = snap2.display.unwrap();
        assert_eq!(d1.units, d2.units);
        assert_eq!(d1.precision, d2.precision);
    }

    /// Stub record with a per-field metadata override on SPD only —
    /// models a C RSET whose get_units/get_graphic_double key on
    /// dbGetFieldIndex (e.g. motorRecord.cc:3156-3361).
    static PER_FIELD_META_FIELDS: &[crate::server::record::FieldDesc] = &[
        crate::server::record::FieldDesc::new("VAL", crate::types::DbFieldType::Double, false),
        crate::server::record::FieldDesc::new("SPD", crate::types::DbFieldType::Double, false),
        crate::server::record::FieldDesc::new("EGU", crate::types::DbFieldType::String, false),
        crate::server::record::FieldDesc::new("PREC", crate::types::DbFieldType::Short, false),
        crate::server::record::FieldDesc::new("HOPR", crate::types::DbFieldType::Double, false),
        crate::server::record::FieldDesc::new("LOPR", crate::types::DbFieldType::Double, false),
    ];

    struct PerFieldMetaRecord;

    impl Record for PerFieldMetaRecord {
        /// Its own type, not `ai`: the fixture serves `SPD`, which no `ai`
        /// declares, and a field is readable only where the record type
        /// declares it (`resolve_field`). Record-level metadata still
        /// populates, because that comes from EGU/PREC/HOPR/LOPR below.
        fn record_type(&self) -> &'static str {
            "per_field_meta"
        }
        fn get_field(&self, name: &str) -> Option<EpicsValue> {
            match name {
                "VAL" | "SPD" => Some(EpicsValue::Double(1.0)),
                "EGU" => Some(EpicsValue::String("mm".into())),
                "PREC" => Some(EpicsValue::Short(3)),
                "HOPR" => Some(EpicsValue::Double(100.0)),
                "LOPR" => Some(EpicsValue::Double(-100.0)),
                _ => None,
            }
        }
        fn put_field(&mut self, name: &str, _value: EpicsValue) -> CaResult<()> {
            Err(CaError::FieldNotFound(name.to_string()))
        }
        fn declared_fields(&self) -> &'static [crate::server::record::FieldDesc] {
            PER_FIELD_META_FIELDS
        }
        fn field_metadata_override(
            &self,
            field: &str,
        ) -> Option<crate::server::record::FieldMetadataOverride> {
            if field != "SPD" {
                return None;
            }
            Some(crate::server::record::FieldMetadataOverride {
                units: Some("mm/sec".into()),
                precision: Some(1),
                disp_limits: Some((5.0, 0.5)),
                ctrl_limits: Some((4.0, 1.0)),
                alarm_limits: Some((9.0, 8.0, -8.0, -9.0)),
            })
        }
    }

    #[test]
    fn field_metadata_override_applies_on_get_and_monitor_paths() {
        let inst = RecordInstance::new("PFM".to_string(), PerFieldMetaRecord);

        // VAL: no override — record-level metadata serves it.
        let snap = inst.snapshot_for_field("VAL").unwrap();
        let d = snap.display.unwrap();
        assert_eq!(d.units, "mm");
        assert_eq!(d.precision, 3);
        assert_eq!(d.upper_disp_limit, 100.0);

        // SPD via the GET path: every member patched over the cache.
        let snap = inst.snapshot_for_field("SPD").unwrap();
        let d = snap.display.unwrap();
        assert_eq!(d.units, "mm/sec");
        assert_eq!(d.precision, 1);
        assert_eq!((d.upper_disp_limit, d.lower_disp_limit), (5.0, 0.5));
        assert_eq!(
            (
                d.upper_alarm_limit,
                d.upper_warning_limit,
                d.lower_warning_limit,
                d.lower_alarm_limit
            ),
            (9.0, 8.0, -8.0, -9.0)
        );
        let c = snap.control.unwrap();
        assert_eq!((c.upper_ctrl_limit, c.lower_ctrl_limit), (4.0, 1.0));

        // SPD via the monitor path: identical override.
        let snap = inst.make_monitor_snapshot("SPD", EpicsValue::Double(2.0), LinkBacking::none());
        let d = snap.display.unwrap();
        assert_eq!(d.units, "mm/sec");
        assert_eq!((d.upper_disp_limit, d.lower_disp_limit), (5.0, 0.5));
        let c = snap.control.unwrap();
        assert_eq!((c.upper_ctrl_limit, c.lower_ctrl_limit), (4.0, 1.0));
    }

    /// Stub modelling the motor monitor() shape (C motorRecord.cc:
    /// 3468-3507): VAL is a setpoint, the MDEL/ADEL deadband tracks
    /// the RBV readback, which advances on every process.
    static READBACK_DEADBAND_FIELDS: &[crate::server::record::FieldDesc] = &[
        crate::server::record::FieldDesc::new("VAL", crate::types::DbFieldType::Double, false),
        crate::server::record::FieldDesc::new("RBV", crate::types::DbFieldType::Double, false),
        crate::server::record::FieldDesc::new("MDEL", crate::types::DbFieldType::Double, false),
        crate::server::record::FieldDesc::new("ADEL", crate::types::DbFieldType::Double, false),
    ];

    struct ReadbackDeadbandRecord {
        val: f64,
        rbv: f64,
        deadband: f64,
    }

    impl Record for ReadbackDeadbandRecord {
        /// `RBV` is a motor field, not an `ai` one, and only a record type
        /// that declares a field can serve it.
        fn record_type(&self) -> &'static str {
            "readback_deadband"
        }
        fn process(&mut self) -> CaResult<crate::server::record::ProcessOutcome> {
            self.rbv += 30.0;
            Ok(crate::server::record::ProcessOutcome::complete())
        }
        fn get_field(&self, name: &str) -> Option<EpicsValue> {
            match name {
                "VAL" => Some(EpicsValue::Double(self.val)),
                "RBV" => Some(EpicsValue::Double(self.rbv)),
                "MDEL" | "ADEL" => Some(EpicsValue::Double(self.deadband)),
                _ => None,
            }
        }
        fn put_field(&mut self, name: &str, value: EpicsValue) -> CaResult<()> {
            match (name, value) {
                ("VAL", EpicsValue::Double(v)) => {
                    self.val = v;
                    Ok(())
                }
                ("MDEL", EpicsValue::Double(v)) => {
                    self.deadband = v;
                    Ok(())
                }
                _ => Err(CaError::FieldNotFound(name.to_string())),
            }
        }
        fn declared_fields(&self) -> &'static [crate::server::record::FieldDesc] {
            READBACK_DEADBAND_FIELDS
        }
        fn monitor_deadband_value(&self) -> Option<EpicsValue> {
            Some(EpicsValue::Double(self.rbv))
        }
        fn monitor_deadband_field(&self) -> &'static str {
            "RBV"
        }
    }

    /// C motor monitor() parity: MDEL/ADEL throttle the deadband
    /// field's (RBV) delivery; VAL posts only when the setpoint
    /// actually changed — not on every readback poll.
    #[test]
    fn deadband_field_routes_readback_and_val_posts_only_on_change() {
        use crate::server::recgbl::EventMask;
        let mut inst = RecordInstance::new(
            "RDB".to_string(),
            ReadbackDeadbandRecord {
                val: 5.0,
                rbv: 0.0,
                deadband: 10.0,
            },
        );
        let _val_rx = inst
            .add_subscriber(
                "VAL",
                1,
                crate::types::DbFieldType::Double,
                EventMask::VALUE.bits(),
            )
            .expect("VAL subscriber");
        let _rbv_rx = inst
            .add_subscriber(
                "RBV",
                2,
                crate::types::DbFieldType::Double,
                EventMask::VALUE.bits(),
            )
            .expect("RBV subscriber");
        let names = |snap: &ProcessSnapshot| {
            snap.changed_fields
                .iter()
                .map(|(n, _, _)| n.clone())
                .collect::<Vec<_>>()
        };

        // Cycle 1 (first publish): RBV fires via the deadband trigger
        // (MLST starts at the NaN never-posted sentinel). VAL must NOT
        // post: `add_subscriber` seeded `last_posted` with the current
        // value (the initial value already went out with EVENT_ADD), and
        // C monitor() posts VAL only when MARKED(M_VAL) — nothing marked
        // it.
        let (snap, _) = inst.process_local().unwrap();
        let n = names(&snap);
        assert!(n.contains(&"RBV".to_string()), "{n:?}");
        assert!(
            !n.contains(&"VAL".to_string()),
            "VAL unchanged since subscribe must not post: {n:?}"
        );

        // Cycle 2: RBV moved past MDEL, VAL unchanged → RBV posted,
        // VAL not re-posted.
        let (snap, _) = inst.process_local().unwrap();
        let n = names(&snap);
        assert!(n.contains(&"RBV".to_string()), "RBV crossed MDEL: {n:?}");
        assert!(
            !n.contains(&"VAL".to_string()),
            "unchanged VAL must not post: {n:?}"
        );

        // Cycle 3: widen the deadband — RBV moves within it → throttled.
        let _ = inst.record.put_field("MDEL", EpicsValue::Double(1000.0));
        let (snap, _) = inst.process_local().unwrap();
        let n = names(&snap);
        assert!(
            !n.contains(&"RBV".to_string()),
            "MDEL must throttle RBV: {n:?}"
        );

        // Cycle 4: setpoint moves while RBV stays inside the deadband →
        // VAL posts via change detection, RBV stays throttled.
        let _ = inst.record.put_field("VAL", EpicsValue::Double(42.0));
        let (snap, _) = inst.process_local().unwrap();
        let n = names(&snap);
        assert!(
            n.contains(&"VAL".to_string()),
            "changed VAL must post: {n:?}"
        );
        assert!(
            !n.contains(&"RBV".to_string()),
            "MDEL must throttle RBV: {n:?}"
        );
    }

    /// A subroutine-less aSub (empty SNAM — the record the PVA monitor
    /// oracle drives as `ORACLE:MONSCAN:ASUB`) mirrors C `do_sub`
    /// (aSubRecord.c:459-465): an empty SNAM returns 0 BEFORE the bad-sub
    /// check, and C `process` (`:224`) runs `prec->val = status = 0` every
    /// cycle. So a periodic scan forces VAL back to 0, and C `monitor()`
    /// (`:414`, `val != oval`) posts nothing — the driven `dbPut`s are the
    /// only VAL events.
    ///
    /// Before the fix the port's "no bound subroutine" branch returned
    /// `S_db_BadSub` and never wrote VAL, so a scanned aSub kept VAL at the
    /// last client put and the deadband gate re-posted it on every scan (the
    /// oracle's 7 updates where C posts 4). This pins both halves: `process`
    /// resets VAL to 0, and a scan of the reset value posts nothing.
    #[test]
    fn subroutineless_asub_process_resets_val_and_stops_scan_overposting() {
        use crate::server::recgbl::EventMask;
        use crate::server::records::asub_record::ASubRecord;

        let mut inst = RecordInstance::new("ASUB".to_string(), ASubRecord::default());
        // The default record: no subroutine bound, SNAM empty.
        assert!(inst.subroutine.is_none());
        let _val_rx = inst
            .add_subscriber(
                "VAL",
                1,
                crate::types::DbFieldType::Long,
                EventMask::VALUE.bits(),
            )
            .expect("VAL subscriber");
        let posts_val =
            |snap: &ProcessSnapshot| snap.changed_fields.iter().any(|(n, _, _)| n == "VAL");

        // A settling scan of the unchanged record: C `do_sub` returns 0 and
        // `process` leaves VAL at 0 (already 0), settling the monitor gate.
        let _ = inst.process_local().unwrap();
        assert_eq!(inst.record.get_field("VAL"), Some(EpicsValue::Long(0)));
        // status 0 -> C `if (!status)` drives every OUT link (aSub's
        // `multi_output_links` gate reads the cycle status); a bad-sub status
        // would suppress all 21.
        assert_eq!(
            inst.record.multi_output_links().len(),
            21,
            "empty-SNAM do_sub status must be 0, not S_db_BadSub"
        );

        // A client caput lands on VAL (DBF_LONG, not process-passive: it posts
        // but does not itself process, leaving VAL non-zero — exactly how the
        // oracle drives the scanned reproducer between scans).
        inst.record.put_field("VAL", EpicsValue::Long(7)).unwrap();

        // The periodic scan processes. C forces VAL back to 0 and posts
        // nothing (val == oval == 0). Before the fix VAL stayed 7 and the scan
        // re-posted it.
        let (snap, _) = inst.process_local().unwrap();
        assert_eq!(
            inst.record.get_field("VAL"),
            Some(EpicsValue::Long(0)),
            "a scan must reset VAL to the do_sub status (0)"
        );
        assert!(
            !posts_val(&snap),
            "a scan that resets VAL to 0 must not re-post it"
        );

        // A second driven put + scan: the monitor marker stays at 0, so no
        // scan ever re-posts the reset value.
        inst.record.put_field("VAL", EpicsValue::Long(7)).unwrap();
        let (snap, _) = inst.process_local().unwrap();
        assert_eq!(inst.record.get_field("VAL"), Some(EpicsValue::Long(0)));
        assert!(
            !posts_val(&snap),
            "repeated scans must not re-post the reset VAL"
        );
    }

    /// Record that names DIFF in `force_posted_fields` (the motor's C
    /// `process_motor_info` unconditional `MARK(M_DIFF)`) while keeping
    /// every value constant — a settled axis parked at a fixed non-zero
    /// following error. VAL is a control: not force-listed, so it must
    /// fall back to change-detection.
    static FORCE_POST_FIELDS: &[crate::server::record::FieldDesc] = &[
        crate::server::record::FieldDesc::new("DIFF", crate::types::DbFieldType::Double, false),
        crate::server::record::FieldDesc::new("VAL", crate::types::DbFieldType::Double, false),
    ];

    struct ForcePostRecord {
        diff: f64,
        val: f64,
    }

    impl Record for ForcePostRecord {
        fn record_type(&self) -> &'static str {
            "force_post"
        }
        fn process(&mut self) -> CaResult<crate::server::record::ProcessOutcome> {
            // Values never change — the readback already matches; only the
            // unconditional MARK should keep DIFF flowing.
            Ok(crate::server::record::ProcessOutcome::complete())
        }
        fn get_field(&self, name: &str) -> Option<EpicsValue> {
            match name {
                "DIFF" => Some(EpicsValue::Double(self.diff)),
                "VAL" => Some(EpicsValue::Double(self.val)),
                _ => None,
            }
        }
        fn put_field(&mut self, name: &str, _value: EpicsValue) -> CaResult<()> {
            Err(CaError::FieldNotFound(name.to_string()))
        }
        fn declared_fields(&self) -> &'static [crate::server::record::FieldDesc] {
            FORCE_POST_FIELDS
        }
        fn force_posted_fields(&self) -> &'static [&'static str] {
            &["DIFF"]
        }
    }

    /// C motorRecord parity: `process_motor_info` MARKs M_DIFF/M_RDIF every
    /// CALLBACK_DATA pass and `monitor()` posts them with `DBE_VAL_LOG`
    /// regardless of change, so a force-posted field re-posts on an
    /// otherwise-idle cycle while an unchanged non-force field does not.
    #[test]
    fn force_posted_field_reposts_unchanged_value_each_cycle() {
        use crate::server::recgbl::EventMask;
        let mut inst = RecordInstance::new(
            "FP".to_string(),
            ForcePostRecord {
                diff: 2.5,
                val: 1.0,
            },
        );
        let _diff_rx = inst
            .add_subscriber(
                "DIFF",
                1,
                crate::types::DbFieldType::Double,
                EventMask::VALUE.bits(),
            )
            .expect("DIFF subscriber");
        let _val_rx = inst
            .add_subscriber(
                "VAL",
                2,
                crate::types::DbFieldType::Double,
                EventMask::VALUE.bits(),
            )
            .expect("VAL subscriber");
        let names = |snap: &ProcessSnapshot| {
            snap.changed_fields
                .iter()
                .map(|(n, _, _)| n.clone())
                .collect::<Vec<_>>()
        };

        // Cycle 1 (first publish): both DIFF and VAL post — last_posted is
        // empty so change-detection treats every subscribed field as new.
        let (snap1, _) = inst.process_local().unwrap();
        assert!(
            names(&snap1).contains(&"DIFF".to_string()),
            "DIFF posts on first publish: {:?}",
            names(&snap1)
        );

        // Cycle 2: nothing changed. VAL (not force-listed) must NOT re-post;
        // DIFF (force-listed) MUST re-post — the C unconditional MARK +
        // DBE_VAL_LOG. This is the divergence MOT-1 closes.
        let (snap2, _) = inst.process_local().unwrap();
        assert!(
            names(&snap2).contains(&"DIFF".to_string()),
            "force-posted DIFF must re-post when unchanged: {:?}",
            names(&snap2)
        );
        assert!(
            !names(&snap2).contains(&"VAL".to_string()),
            "an unchanged non-force field must not re-post: {:?}",
            names(&snap2)
        );
        // The forced re-post carries DBE_VALUE|DBE_LOG (no alarm bits this
        // cycle), matching C `monitor_mask | DBE_VAL_LOG` with monitor_mask=0.
        let diff_mask = snap2
            .changed_fields
            .iter()
            .find(|(n, _, _)| n == "DIFF")
            .map(|(_, _, m)| *m)
            .expect("DIFF post present");
        assert_eq!(
            diff_mask.bits(),
            (EventMask::VALUE | EventMask::LOG).bits(),
            "forced re-post mask is DBE_VAL_LOG"
        );
    }

    /// Record that names S1 in `log_swept_fields` (the scaler's idle
    /// `monitor()` DBE_LOG sweep) while keeping every value constant. S2
    /// is a control: subscribed but NOT swept, so an unchanged S2 must
    /// not re-post. Neither field is the primary `VAL`, so the default
    /// deadband field resolves to nothing and does not confound the test.
    static LOG_SWEEP_FIELDS: &[crate::server::record::FieldDesc] = &[
        crate::server::record::FieldDesc::new("S1", crate::types::DbFieldType::Long, false),
        crate::server::record::FieldDesc::new("S2", crate::types::DbFieldType::Long, false),
    ];

    struct LogSweepRecord {
        s1: i32,
        s2: i32,
    }

    impl Record for LogSweepRecord {
        fn record_type(&self) -> &'static str {
            "scaler"
        }
        fn process(&mut self) -> CaResult<crate::server::record::ProcessOutcome> {
            // Counts never change — only the unconditional idle LOG sweep
            // should keep S1 flowing to a DBE_LOG (archiver) subscriber.
            Ok(crate::server::record::ProcessOutcome::complete())
        }
        fn get_field(&self, name: &str) -> Option<EpicsValue> {
            match name {
                "S1" => Some(EpicsValue::Long(self.s1)),
                "S2" => Some(EpicsValue::Long(self.s2)),
                _ => None,
            }
        }
        fn put_field(&mut self, name: &str, value: EpicsValue) -> CaResult<()> {
            match (name, value) {
                ("S1", EpicsValue::Long(v)) => {
                    self.s1 = v;
                    Ok(())
                }
                ("S2", EpicsValue::Long(v)) => {
                    self.s2 = v;
                    Ok(())
                }
                _ => Err(CaError::FieldNotFound(name.to_string())),
            }
        }
        fn declared_fields(&self) -> &'static [crate::server::record::FieldDesc] {
            LOG_SWEEP_FIELDS
        }
        fn log_swept_fields(&self) -> &'static [&'static str] {
            &["S1"]
        }
    }

    /// C `scalerRecord.c::monitor():757-773` sweeps each active channel with a
    /// literal `DBE_LOG` on every cycle it runs, unconditionally — the sweep is
    /// INDEPENDENT of the change post, not an alternative to it (R12-62). So an
    /// UNCHANGED swept field posts `DBE_LOG` only, and a CHANGED swept field
    /// posts TWICE on that one cycle: once by change-detection, and once by the
    /// sweep with `DBE_LOG`. (In C's scaler those two are `updateCounts()`'s
    /// `DBE_VALUE` at `:582` and `monitor()`'s `DBE_LOG` at `:771`.) A
    /// non-swept field never re-posts when unchanged. `add_subscriber` seeds
    /// `last_posted` with the current value (the initial value goes out via
    /// EVENT_ADD), so a freshly subscribed unchanged field already takes the
    /// sweep path on cycle 1.
    #[test]
    fn log_swept_field_reposts_unchanged_with_log_mask_only() {
        use crate::server::recgbl::EventMask;
        let mut inst = RecordInstance::new("SW".to_string(), LogSweepRecord { s1: 7, s2: 9 });
        let _s1_rx = inst
            .add_subscriber(
                "S1",
                1,
                crate::types::DbFieldType::Long,
                EventMask::LOG.bits(),
            )
            .expect("S1 subscriber");
        let _s2_rx = inst
            .add_subscriber(
                "S2",
                2,
                crate::types::DbFieldType::Long,
                EventMask::VALUE.bits(),
            )
            .expect("S2 subscriber");
        let names = |snap: &ProcessSnapshot| {
            snap.changed_fields
                .iter()
                .map(|(n, _, _)| n.clone())
                .collect::<Vec<_>>()
        };
        let count_of = |snap: &ProcessSnapshot, f: &str| {
            snap.changed_fields
                .iter()
                .filter(|(n, _, _)| n == f)
                .count()
        };
        let mask_of = |snap: &ProcessSnapshot, f: &str| {
            snap.changed_fields
                .iter()
                .find(|(n, _, _)| n == f)
                .map(|(_, _, m)| *m)
        };

        // Cycle 1: nothing changed since subscribe. S1 (swept) re-posts
        // with DBE_LOG ONLY; S2 (not swept) must NOT re-post.
        let (snap1, _) = inst.process_local().unwrap();
        assert!(
            names(&snap1).contains(&"S1".to_string()),
            "log-swept S1 must re-post when unchanged: {:?}",
            names(&snap1)
        );
        assert!(
            !names(&snap1).contains(&"S2".to_string()),
            "unchanged non-swept S2 must not re-post: {:?}",
            names(&snap1)
        );
        // DBE_LOG, plus the DBE_ALARM of this cycle's transition: a record
        // starts UDF/INVALID and its first process clears that, so cycle 1 IS an
        // alarm transition (CBUG-B19 — C's sweep drops the alarm bit; this
        // assertion used to require a bare DBE_LOG). No DBE_VALUE either way:
        // the counts have not moved.
        assert_eq!(
            mask_of(&snap1, "S1").unwrap().bits(),
            (EventMask::LOG | EventMask::ALARM).bits(),
            "idle sweep posts DBE_LOG + the alarm transition, never DBE_VALUE"
        );

        // Cycle 2: S1's count changed. Change-detection delivers it, and the
        // sweep delivers it AGAIN with DBE_LOG — the two C `db_post_events`
        // calls of the count-completion cycle.
        inst.record.put_field("S1", EpicsValue::Long(8)).unwrap();
        let (snap2, _) = inst.process_local().unwrap();
        assert_eq!(
            count_of(&snap2, "S1"),
            2,
            "a changed swept field posts twice — change post + independent \
             DBE_LOG sweep: {:?}",
            snap2.changed_fields
        );
        let s1_masks: Vec<u16> = snap2
            .changed_fields
            .iter()
            .filter(|(n, _, _)| n == "S1")
            .map(|(_, _, m)| m.bits())
            .collect();
        assert_eq!(
            s1_masks,
            vec![
                (EventMask::VALUE | EventMask::LOG).bits(),
                EventMask::LOG.bits()
            ],
            "change post first (VALUE|LOG here — this stub is not a \
             value_only_change_fields record), then the sweep's literal DBE_LOG"
        );

        // Cycle 3: unchanged again — back to the DBE_LOG-only sweep.
        let (snap3, _) = inst.process_local().unwrap();
        assert_eq!(
            mask_of(&snap3, "S1").unwrap().bits(),
            EventMask::LOG.bits(),
            "unchanged-again S1 returns to the DBE_LOG-only sweep"
        );
    }

    /// A log-swept record that can raise an alarm on demand — the scaler's
    /// `do_alarm()` (scalerRecord.c:745-755) in miniature.
    static ALARMING_LOG_SWEEP_FIELDS: &[crate::server::record::FieldDesc] =
        &[crate::server::record::FieldDesc::new(
            "S1",
            crate::types::DbFieldType::Long,
            false,
        )];

    struct AlarmingLogSweepRecord {
        s1: i32,
        alarm: bool,
    }

    impl Record for AlarmingLogSweepRecord {
        fn record_type(&self) -> &'static str {
            "scaler"
        }
        fn process(&mut self) -> CaResult<crate::server::record::ProcessOutcome> {
            Ok(crate::server::record::ProcessOutcome::complete())
        }
        /// This fixture drives its alarm purely through `check_alarms`
        /// (`self.alarm`), so it must NOT also raise the central UDF alarm —
        /// otherwise the born `udf = 1` pins severity at INVALID every cycle
        /// and there is never a real NO_ALARM → INVALID transition to test.
        /// (Before `rec_gbl_check_udf` stopped fabricating a UDF message, the
        /// ALARM bit this test asserts came from that fabricated amsg
        /// flipping to "" — an artifact, not the severity transition the
        /// test name and comments describe.) With no UDF alarm, cycle 1
        /// genuinely clears the born UDF/INVALID to NO_ALARM, and the
        /// `self.alarm` cycle is a true severity transition.
        fn raises_udf_alarm(&self) -> bool {
            false
        }
        fn check_alarms(&mut self, common: &mut crate::server::record::CommonFields) {
            if self.alarm {
                crate::server::recgbl::rec_gbl_set_sevr(
                    common,
                    crate::server::recgbl::alarm_status::UDF_ALARM,
                    crate::server::record::AlarmSeverity::Invalid,
                );
            }
        }
        fn get_field(&self, name: &str) -> Option<EpicsValue> {
            match name {
                "S1" => Some(EpicsValue::Long(self.s1)),
                _ => None,
            }
        }
        fn put_field(&mut self, name: &str, value: EpicsValue) -> CaResult<()> {
            match (name, value) {
                ("S1", EpicsValue::Long(v)) => {
                    self.s1 = v;
                    Ok(())
                }
                _ => Err(CaError::FieldNotFound(name.to_string())),
            }
        }
        fn declared_fields(&self) -> &'static [crate::server::record::FieldDesc] {
            ALARMING_LOG_SWEEP_FIELDS
        }
        fn log_swept_fields(&self) -> &'static [&'static str] {
            &["S1"]
        }
        fn as_any_mut(&mut self) -> Option<&mut dyn std::any::Any> {
            Some(self)
        }
    }

    /// CBUG-B19 — the sweep post carries the alarm-transition bits.
    ///
    /// DEVIATION from C, deliberate. C's scaler `monitor()` computes
    /// `monitor_mask = recGblResetAlarms(pscal)` (scalerRecord.c:764), ORs
    /// `DBE_VALUE|DBE_LOG` into it (`:766`), and then posts every `Sn` with a
    /// LITERAL `DBE_LOG` (`:771`) — `monitor_mask` is assigned, OR-ed, and never
    /// read. The alarm bit that `recGblResetAlarms` returns is exactly what every
    /// other record ORs into its value posts, so C drops it: a client subscribed
    /// to `Sn` with DBE_ALARM receives NOTHING on a severity transition.
    ///
    /// The DBE_VALUE half of C's dead `|=` is deliberately not resurrected — the
    /// sweep is unconditional, so a VALUE bit here would fire a value event on
    /// every idle scan whether or not the counts moved. The first assertion pins
    /// that.
    #[test]
    fn b19_log_swept_field_carries_the_alarm_transition_bits() {
        use crate::server::recgbl::EventMask;
        let mut inst = RecordInstance::new(
            "SW".to_string(),
            AlarmingLogSweepRecord {
                s1: 7,
                alarm: false,
            },
        );
        let _s1_rx = inst
            .add_subscriber(
                "S1",
                1,
                crate::types::DbFieldType::Long,
                (EventMask::LOG | EventMask::ALARM).bits(),
            )
            .expect("S1 subscriber");
        let mask_of = |snap: &ProcessSnapshot, f: &str| {
            snap.changed_fields
                .iter()
                .find(|(n, _, _)| n == f)
                .map(|(_, _, m)| *m)
        };

        // Cycle 1 clears the record's initial UDF/INVALID alarm, which is itself
        // a transition; cycle 2 is the quiet baseline. The sweep is then DBE_LOG
        // alone — in particular NOT DBE_VALUE, since the counts have not moved.
        let _ = inst.process_local().unwrap();
        let (snap1, _) = inst.process_local().unwrap();
        assert_eq!(
            mask_of(&snap1, "S1").unwrap().bits(),
            EventMask::LOG.bits(),
            "no alarm transition → the sweep is DBE_LOG only"
        );

        // The alarm fires: severity moves NO_ALARM → INVALID, so this cycle's
        // posts carry DBE_ALARM. C posts DBE_LOG here and the alarm subscriber
        // learns nothing.
        if let Some(r) = inst
            .record
            .as_any_mut()
            .and_then(|a| a.downcast_mut::<AlarmingLogSweepRecord>())
        {
            r.alarm = true;
        }
        let (snap2, _) = inst.process_local().unwrap();
        assert_eq!(
            mask_of(&snap2, "S1").unwrap().bits(),
            (EventMask::LOG | EventMask::ALARM).bits(),
            "the severity transition must reach the swept field (C drops it)"
        );

        // Severity stays INVALID: no transition, so no alarm bit — the sweep is
        // DBE_LOG again.
        let (snap3, _) = inst.process_local().unwrap();
        assert_eq!(
            mask_of(&snap3, "S1").unwrap().bits(),
            EventMask::LOG.bits(),
            "a steady severity is not a transition"
        );
    }

    /// Stub record that simulates a record whose process() mutates an
    /// internal metadata field. Used to verify that the
    /// `Record::took_metadata_change()` hook actually triggers cache
    /// invalidation in `process_local()`.
    struct MutatingMetaRecord {
        val: f64,
        egu: String,
        took_change: bool,
    }

    impl Record for MutatingMetaRecord {
        fn record_type(&self) -> &'static str {
            "ai" // pretend to be ai so populate_display_info populates EGU
        }
        fn process(&mut self) -> CaResult<crate::server::record::ProcessOutcome> {
            // Simulate dynamic metadata change inside processing
            self.egu = "kV".into();
            self.took_change = true;
            Ok(crate::server::record::ProcessOutcome::complete())
        }
        fn get_field(&self, name: &str) -> Option<EpicsValue> {
            match name {
                "VAL" => Some(EpicsValue::Double(self.val)),
                "EGU" => Some(EpicsValue::String(self.egu.clone().into())),
                "PREC" => Some(EpicsValue::Short(0)),
                "HOPR" => Some(EpicsValue::Double(0.0)),
                "LOPR" => Some(EpicsValue::Double(0.0)),
                _ => None,
            }
        }
        fn put_field(&mut self, name: &str, value: EpicsValue) -> CaResult<()> {
            match (name, value) {
                ("VAL", EpicsValue::Double(v)) => {
                    self.val = v;
                    Ok(())
                }
                ("EGU", EpicsValue::String(s)) => {
                    self.egu = s.as_str_lossy().into_owned();
                    Ok(())
                }
                _ => Err(CaError::FieldNotFound(name.to_string())),
            }
        }
        fn declared_fields(&self) -> &'static [crate::server::record::FieldDesc] {
            &[]
        }
        fn took_metadata_change(&mut self) -> bool {
            let was = self.took_change;
            self.took_change = false; // reset after reporting
            was
        }
    }

    #[test]
    fn process_local_invalidates_cache_on_took_metadata_change() {
        let mut inst = RecordInstance::new(
            "MUT".to_string(),
            MutatingMetaRecord {
                val: 1.0,
                egu: "V".to_string(),
                took_change: false,
            },
        );

        // Build the cache once with the original EGU
        let snap1 = inst.snapshot_for_field("VAL").unwrap();
        assert_eq!(snap1.display.unwrap().units, "V");
        assert!(inst.metadata_cache.lock().unwrap().is_some());

        // Run process_local — the stub record sets took_change inside process()
        let _ = inst.process_local();

        // Cache should now be invalidated (took_metadata_change returned true)
        assert!(
            inst.metadata_cache.lock().unwrap().is_none(),
            "process_local should invalidate cache when took_metadata_change is true"
        );

        // Next snapshot picks up the new EGU
        let snap2 = inst.snapshot_for_field("VAL").unwrap();
        assert_eq!(snap2.display.unwrap().units, "kV");
    }

    /// Stub record that does NOT mutate metadata fields. Verifies the
    /// default `took_metadata_change` returns false and the cache stays.
    struct StableMetaRecord {
        val: f64,
    }
    impl Record for StableMetaRecord {
        fn record_type(&self) -> &'static str {
            "ai"
        }
        fn process(&mut self) -> CaResult<crate::server::record::ProcessOutcome> {
            self.val += 1.0;
            Ok(crate::server::record::ProcessOutcome::complete())
        }
        fn get_field(&self, name: &str) -> Option<EpicsValue> {
            match name {
                "VAL" => Some(EpicsValue::Double(self.val)),
                "EGU" => Some(EpicsValue::String("V".into())),
                "PREC" => Some(EpicsValue::Short(0)),
                "HOPR" => Some(EpicsValue::Double(0.0)),
                "LOPR" => Some(EpicsValue::Double(0.0)),
                _ => None,
            }
        }
        fn put_field(&mut self, _: &str, _: EpicsValue) -> CaResult<()> {
            Ok(())
        }
        fn declared_fields(&self) -> &'static [crate::server::record::FieldDesc] {
            &[]
        }
        // took_metadata_change uses default impl (returns false)
    }

    #[test]
    fn process_local_keeps_cache_when_no_metadata_change() {
        let mut inst = RecordInstance::new("STABLE".to_string(), StableMetaRecord { val: 0.0 });

        let _ = inst.snapshot_for_field("VAL");
        assert!(inst.metadata_cache.lock().unwrap().is_some());

        // Run process_local several times — cache should remain intact
        let _ = inst.process_local();
        assert!(inst.metadata_cache.lock().unwrap().is_some());
        let _ = inst.process_local();
        assert!(inst.metadata_cache.lock().unwrap().is_some());
        let _ = inst.process_local();
        assert!(inst.metadata_cache.lock().unwrap().is_some());
    }

    // ── Regression: DBE_PROPERTY event delivery boundaries ──────────────

    /// Subscribe `VAL` for PROPERTY, put `field`, run the post gate, and report
    /// whether an event was delivered. `put` must differ from the field's
    /// current value or the change-detection suppresses the post either way.
    fn property_event_on_put<R: Record>(rec: R, field: &str, put: EpicsValue) -> bool {
        use crate::server::recgbl::EventMask;
        let mut inst = RecordInstance::new("PROPGATE".to_string(), rec);
        let mut rx = inst
            .add_subscriber(
                "VAL",
                1,
                crate::types::DbFieldType::Double,
                EventMask::PROPERTY.bits(),
            )
            .expect("subscriber added");
        let prev = inst.record.get_field(field);
        assert_ne!(prev.as_ref(), Some(&put), "{field}: put must be a change");
        inst.record.put_field(field, put).expect("put accepted");
        inst.notify_field_written_if_changed(field, prev.as_ref(), LinkBacking::none());
        rx.try_recv().is_ok()
    }

    /// Boundary: `prop(YES)`. `histogramRecord.dbd.pod` declares ULIM
    /// `special(SPC_RESET)` + `prop(YES)`, so C's `dbPut` sets
    /// `propertyUpdate` (dbAccess.c:1330) and posts DBE_PROPERTY. ULIM is
    /// nobody's cache source — it reaches the wire through the live
    /// `apply_field_metadata_override` — so a set keyed on cache sources
    /// cannot answer this, which is why the gate reads the declaration.
    #[test]
    fn prop_yes_field_posts_property_event() {
        use crate::server::records::histogram::HistogramRecord;
        assert!(
            property_event_on_put(
                HistogramRecord::default(),
                "ULIM",
                EpicsValue::Double(100.0)
            ),
            "histogram.ULIM is prop(YES) — a changed put must post DBE_PROPERTY"
        );
    }

    /// Boundary: `pp(TRUE)` without `prop`. `biRecord.dbd.pod` declares ZSV
    /// `pp(TRUE)`, `menu(menuAlarmSevr)` and no `prop`, so C's
    /// `paddr->pfldDes->prop` is 0 and no property event is posted.
    #[test]
    fn pp_true_without_prop_posts_no_property_event() {
        use crate::server::records::bi::BiRecord;
        assert!(
            !property_event_on_put(BiRecord::default(), "ZSV", EpicsValue::Short(2)),
            "bi.ZSV is pp(TRUE) but not prop(YES) — it must post no DBE_PROPERTY"
        );
    }

    /// Boundary 1: metadata field written with a CHANGED value, subscriber
    /// mask includes PROPERTY → subscriber receives an event.
    /// Mirrors C dbAccess.c:1395-1396 `if (propertyUpdate && !status)`
    /// `db_post_events(precord,NULL,DBE_PROPERTY)`.
    #[test]
    fn r47_property_event_delivered_on_changed_metadata() {
        use crate::server::recgbl::EventMask;
        let mut inst = ai_instance();
        let mut rx = inst
            .add_subscriber(
                "VAL",
                1,
                crate::types::DbFieldType::Double,
                EventMask::PROPERTY.bits(),
            )
            .expect("subscriber added");

        let prev = inst.record.get_field("EGU"); // "degC"
        let _ = inst
            .record
            .put_field("EGU", EpicsValue::String("kPa".into()));
        inst.notify_field_written_if_changed("EGU", prev.as_ref(), LinkBacking::none());

        assert!(
            rx.try_recv().is_ok(),
            "PROPERTY subscriber must receive event when metadata field changes"
        );
    }

    /// Boundary 2: same metadata field written with the SAME value → NO event.
    /// Matches C suppression at dbAccess.c:1379-1383 and the `prev != now` gate.
    #[test]
    fn r47_no_event_on_unchanged_metadata() {
        use crate::server::recgbl::EventMask;
        let mut inst = ai_instance();
        let mut rx = inst
            .add_subscriber(
                "VAL",
                1,
                crate::types::DbFieldType::Double,
                EventMask::PROPERTY.bits(),
            )
            .expect("subscriber added");

        let prev = inst.record.get_field("EGU"); // "degC"
        // Write the same value — no change
        let _ = inst.record.put_field("EGU", prev.clone().unwrap());
        inst.notify_field_written_if_changed("EGU", prev.as_ref(), LinkBacking::none());

        assert!(
            rx.try_recv().is_err(),
            "PROPERTY subscriber must NOT receive event when metadata value is unchanged"
        );
    }

    /// Boundary 3: VALUE-only subscriber (no PROPERTY bit) receives NO event
    /// from a metadata write, even when the field value changed.
    #[test]
    fn r47_value_only_subscriber_no_event_on_metadata_write() {
        use crate::server::recgbl::EventMask;
        let mut inst = ai_instance();
        let mut rx = inst
            .add_subscriber(
                "VAL",
                1,
                crate::types::DbFieldType::Double,
                EventMask::VALUE.bits(),
            )
            .expect("subscriber added");

        let prev = inst.record.get_field("EGU"); // "degC"
        let _ = inst
            .record
            .put_field("EGU", EpicsValue::String("kPa".into()));
        inst.notify_field_written_if_changed("EGU", prev.as_ref(), LinkBacking::none());

        assert!(
            rx.try_recv().is_err(),
            "VALUE-only subscriber must NOT receive event from a metadata write"
        );
    }

    /// Boundary 4 (took_metadata_change path): PROPERTY subscriber receives
    /// event after process_local() when the record reports a metadata change.
    #[test]
    fn r47_process_local_property_event_on_took_metadata_change() {
        use crate::server::recgbl::EventMask;
        let mut inst = RecordInstance::new(
            "MUT2".to_string(),
            MutatingMetaRecord {
                val: 1.0,
                egu: "V".to_string(),
                took_change: false,
            },
        );
        let mut rx = inst
            .add_subscriber(
                "VAL",
                1,
                crate::types::DbFieldType::Double,
                EventMask::PROPERTY.bits(),
            )
            .expect("subscriber added");

        // process() sets took_change = true and updates egu to "kV"
        let _ = inst.process_local();

        assert!(
            rx.try_recv().is_ok(),
            "PROPERTY subscriber must receive event after process_local reports took_metadata_change"
        );
    }
}

#[cfg(test)]
mod aftc_filter_tests {
    //! Tests for the shared AFTC alarm-range filter
    //! (`records::alarm_filter::aftc_filter`) as driven by
    //! `evaluate_analog_alarm`. Pure-function tests: no record instance
    //! needed — the filter is a stateless transform of (raw_alarm, aftc,
    //! afvl_in, t_last, t_now). Algorithm provenance: 2009 EPICS
    //! Codeathon (epics-base `824d37811`), C `aiRecord.c:355-401`.

    use crate::server::records::alarm_filter::aftc_filter;
    use std::time::{Duration, SystemTime};

    fn at(secs: f64) -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs_f64(secs)
    }

    #[test]
    fn disabled_when_aftc_le_zero() {
        // aftc=0 means filter disabled — pass-through.
        let (out, afvl) = aftc_filter(2, 0.0, 0.0, at(0.0), at(1.0));
        assert_eq!(out, 2);
        assert_eq!(afvl, 0.0);
    }

    #[test]
    fn initial_sample_seeds_state_unchanged_alarm() {
        // afvl=0 means first sample after enable — alarm passes through
        // and accumulator seeds with the raw severity.
        let (out, afvl) = aftc_filter(2, 3.0, 0.0, at(0.0), at(0.5));
        assert_eq!(out, 2);
        assert_eq!(afvl, 2.0);
    }

    #[test]
    fn raises_alarm_only_after_full_time_constant() {
        // Single-step heuristic: with `aftc = 3s` and `dt = 0.1s`, alpha
        // ≈ 0.967, so a one-shot raw_alarm=2 against afvl=0.0 should not
        // produce alarm=2 yet — the filter must hold off until the
        // accumulator crosses the threshold.
        // Seed with afvl=0.01 (tiny prior, simulating "almost no alarm
        // yet"); the filter must keep alarm at 0 after one short tick.
        let (out, afvl) = aftc_filter(2, 3.0, 0.01, at(0.0), at(0.1));
        assert_eq!(out, 0, "filter should suppress alarm rise on a 0.1s tick");
        assert!(afvl > 0.0 && afvl < 2.0);
    }

    #[test]
    fn dt_zero_is_no_op() {
        // Two evaluations at the same instant produce no filter advance.
        let (out, afvl) = aftc_filter(2, 3.0, 1.5, at(0.0), at(0.0));
        assert_eq!(out, 1); // floor(|1.5|) = 1
        assert_eq!(afvl, 1.5);
    }

    #[test]
    fn long_steady_state_converges_to_alarm() {
        // After many steps with raw_alarm=2 and dt much smaller than aftc,
        // the accumulator must converge towards 2.
        let aftc = 1.0;
        let mut afvl = 0.0;
        let mut last = at(0.0);
        let mut alarm = 0;
        for i in 1..=100 {
            let now = at(i as f64 * 0.05);
            let (out, new_afvl) = aftc_filter(2, aftc, afvl, last, now);
            alarm = out;
            afvl = new_afvl;
            last = now;
        }
        assert_eq!(
            alarm, 2,
            "after 5 s of steady raw=2 with aftc=1 s, output must reach 2"
        );
        assert!(afvl.abs() >= 1.99 && afvl.abs() <= 2.0);
    }
}

#[cfg(test)]
mod check_deadband_tests {
    use super::check_deadband;

    const NAN: f64 = f64::NAN;
    const INF: f64 = f64::INFINITY;

    /// C's own test for this function, transcribed:
    /// `modules/database/test/ioc/db/recGblCheckDeadbandTest.c` runs all 19
    /// (oldval, newval) pairs it can build from {below-band, above-band,
    /// unchanged, -0.0, NaN, +inf, -inf} against deadbands -1, 0 and 1.5, and
    /// carries the expected mask for each of the 57 cells. Transcribing the
    /// table rather than writing cases per story is what keeps the pairs no C
    /// branch matches — `(NaN, NaN)` and same-signed infinity — from being
    /// dropped, since they are exactly the ones a reader is tempted to fold
    /// into "not comparable, so post".
    #[test]
    fn matches_the_c_recgblcheckdeadband_truth_table() {
        // t_SetValues: [oldval, newval]
        let pairs: [(f64, f64); 19] = [
            (1.0, 2.0),
            (0.0, 2.0),
            (0.0, 0.0),
            (-0.0, 0.0),
            (1.0, NAN),
            (1.0, INF),
            (1.0, -INF),
            (NAN, 1.0),
            (NAN, NAN),
            (NAN, INF),
            (NAN, -INF),
            (INF, 1.0),
            (INF, NAN),
            (INF, INF),
            (INF, -INF),
            (-INF, 1.0),
            (-INF, NAN),
            (-INF, INF),
            (-INF, -INF),
        ];
        // t_ExpectedUpdates, one row per deadband in t_Deadband.
        let expected: [(f64, [bool; 19]); 3] = [
            (
                -1.0,
                [
                    true, true, true, true, true, true, true, true, true, true, true, true, true,
                    true, true, true, true, true, true,
                ],
            ),
            (
                0.0,
                [
                    true, true, false, false, true, true, true, true, false, true, true, true,
                    true, false, true, true, true, true, false,
                ],
            ),
            (
                1.5,
                [
                    false, true, false, false, true, true, true, true, false, true, true, true,
                    true, false, true, true, true, true, false,
                ],
            ),
        ];

        for (deadband, row) in expected {
            for (i, ((oldval, newval), want)) in pairs.iter().zip(row).enumerate() {
                assert_eq!(
                    check_deadband(*newval, Some(*oldval), deadband),
                    want,
                    "C pattern {i}: deadband={deadband} oldval={oldval} newval={newval}"
                );
            }
        }
    }

    /// The port-only state: a record type with no MLST/ALST cell has posted
    /// nothing, so the first comparison has no baseline and must fire whatever
    /// the value and the deadband are — including the value C's table says
    /// would not fire against an equal baseline.
    #[test]
    fn never_posted_fires_regardless_of_value_or_deadband() {
        for value in [0.0, 1.0, NAN, INF, -INF] {
            for deadband in [-1.0, 0.0, 1.5] {
                assert!(
                    check_deadband(value, None, deadband),
                    "never-posted must fire: value={value} deadband={deadband}"
                );
            }
        }
    }
}

#[cfg(test)]
mod common_field_dbload_tests {
    use super::*;
    use crate::server::records::ai::AiRecord;

    /// The db loader feeds every common field to `put_common_field` as an
    /// `EpicsValue::String`. Each numeric/menu common field directive must
    /// take effect at load — both the integer form (`field(PHAS, "1")`) and
    /// the menu-label form (`field(PRIO, "HIGH")`, `field(DISS, "MAJOR")`) —
    /// rather than being silently dropped because the arm matched only its
    /// typed variant. One assertion per affected common-field arm.
    #[test]
    fn db_loaded_string_common_fields_take_effect() {
        let mut inst = RecordInstance::new("REC".to_string(), AiRecord::default());
        let put = |inst: &mut RecordInstance, f: &str, v: &str| {
            inst.put_common_field_db_load(f, EpicsValue::String(v.into()))
                .unwrap_or_else(|e| panic!("put_common_field_db_load({f}, {v:?}) failed: {e}"));
        };

        // Integer-valued directives.
        put(&mut inst, "PHAS", "1");
        assert_eq!(inst.common.phas, 1, "field(PHAS, \"1\")");
        put(&mut inst, "TSE", "-2");
        assert_eq!(inst.common.tse, -2, "field(TSE, \"-2\")");
        put(&mut inst, "DISV", "1");
        assert_eq!(inst.common.disv, 1, "field(DISV, \"1\")");
        put(&mut inst, "DISA", "1");
        assert_eq!(inst.common.disa, 1, "field(DISA, \"1\")");
        put(&mut inst, "LCNT", "3");
        assert_eq!(inst.common.lcnt, 3, "field(LCNT, \"3\")");
        put(&mut inst, "DISP", "1");
        assert!(inst.common.disp != 0, "field(DISP, \"1\")");
        put(&mut inst, "UDF", "0");
        assert!(inst.common.udf == 0, "field(UDF, \"0\")");

        // Menu-label directives (resolved via the one menu converter).
        put(&mut inst, "PRIO", "HIGH");
        assert_eq!(inst.common.prio, 2, "field(PRIO, \"HIGH\")");
        put(&mut inst, "DISS", "MAJOR");
        assert_eq!(
            inst.common.diss,
            AlarmSeverity::Major as i16,
            "field(DISS, \"MAJOR\")"
        );
        put(&mut inst, "UDFS", "NO_ALARM");
        assert_eq!(
            inst.common.udfs,
            AlarmSeverity::NoAlarm as i16,
            "field(UDFS, \"NO_ALARM\")"
        );
        put(&mut inst, "ACKT", "NO");
        assert!(!inst.common.ackt, "field(ACKT, \"NO\")");

        // Numeric form of a menu field still works (field(PRIO, "0")).
        put(&mut inst, "PRIO", "0");
        assert_eq!(inst.common.prio, 0, "field(PRIO, \"0\")");

        // A String-typed common field is untouched by the coercion.
        put(&mut inst, "DESC", "a description");
        assert_eq!(inst.common.desc.as_str_lossy().as_ref(), "a description");
    }
}

#[cfg(test)]
mod declared_override_tests {
    use super::*;
    use crate::server::records::dfanout::DfanoutRecord;

    /// A field `dfanout`'s `.dbd` DECLARES (HOPR/LOPR/PREC/EGU) but the
    /// `DfanoutRecord` struct models no storage for: a put must be ACCEPTED and
    /// stored (C `dbPut` writes it into record memory), and a later
    /// `resolve_field` must serve the written value — not the `.dbd` initial.
    #[test]
    fn declared_but_unmodeled_field_put_is_stored_and_served() {
        let mut inst = RecordInstance::new("DF".to_string(), DfanoutRecord::default());

        // Untouched: reads its declared default (initial / type-zero), NOT an
        // error, and the override store is empty.
        assert_eq!(inst.resolve_field("HOPR"), Some(EpicsValue::Double(0.0)));
        assert!(inst.declared_overrides.is_empty());

        // DBF_DOUBLE, DBF_SHORT and DBF_STRING declared metadata fields all
        // land, coerced to the declared type.
        inst.put_common_field("HOPR", EpicsValue::String("10".into()))
            .expect("caput dfanout.HOPR 10 must be accepted");
        inst.put_common_field("PREC", EpicsValue::String("3".into()))
            .expect("caput dfanout.PREC 3 must be accepted");
        inst.put_common_field("EGU", EpicsValue::String("volts".into()))
            .expect("caput dfanout.EGU volts must be accepted");

        assert_eq!(inst.resolve_field("HOPR"), Some(EpicsValue::Double(10.0)));
        assert_eq!(inst.resolve_field("PREC"), Some(EpicsValue::Short(3)));
        assert_eq!(
            inst.resolve_field("EGU"),
            Some(EpicsValue::String("volts".into()))
        );
        // Case-insensitive key: the lower-case read reaches the same slot.
        assert_eq!(inst.resolve_field("hopr"), Some(EpicsValue::Double(10.0)));
    }

    /// The declared type's C range rules apply through the write-side coercion
    /// owner: `caput dfanout.PREC 99999` into a `DBF_SHORT` is REFUSED (C
    /// `epicsParseInt16` overflow → `S_db_badField`), and the field keeps its
    /// prior value — never wraps to a garbage `Short`.
    #[test]
    fn declared_override_honors_declared_type_range() {
        let mut inst = RecordInstance::new("DF".to_string(), DfanoutRecord::default());
        inst.put_common_field("PREC", EpicsValue::String("3".into()))
            .expect("in-range PREC accepted");
        assert!(
            inst.put_common_field("PREC", EpicsValue::String("99999".into()))
                .is_err(),
            "PREC 99999 overflows DBF_SHORT and must be refused"
        );
        assert!(
            inst.put_common_field("PREC", EpicsValue::String("abc".into()))
                .is_err(),
            "non-numeric PREC must be refused"
        );
        // The refused puts left the accepted value intact.
        assert_eq!(inst.resolve_field("PREC"), Some(EpicsValue::Short(3)));
    }

    /// An UNDECLARED field name is still `FieldNotFound` — the override store
    /// captures only fields with a real `dbFldDes`, so a misspelled field is
    /// refused exactly as C's `dbNameToAddr` refuses it.
    #[test]
    fn undeclared_field_is_still_not_found() {
        let mut inst = RecordInstance::new("DF".to_string(), DfanoutRecord::default());
        assert!(matches!(
            inst.put_common_field("XYZZY", EpicsValue::String("1".into())),
            Err(CaError::FieldNotFound(_))
        ));
        assert!(inst.declared_overrides.is_empty());
    }

    /// A PARTIALLY modeled field — one the record SERVES via `get_field` but
    /// has no `put_field` arm for (`calcout.PVAL` → `self.pval`) — must NOT
    /// land in the override map: doing so would place the value where
    /// `resolve_field` (which reads `get_field` first) never sees it, a silent
    /// write loss. The override is only for fields the record serves nothing
    /// for; a partially modeled field's put is the record's own concern.
    #[test]
    fn partially_modeled_field_is_not_captured_by_override() {
        use crate::server::records::calcout::CalcoutRecord;
        let mut inst = RecordInstance::new("CO".to_string(), CalcoutRecord::default());
        // PVAL is served by the record (its own storage), so it is not stored
        // in the override map; the map stays empty and no ghost cell shadows
        // the record's read.
        let _ = inst.put_common_field("PVAL", EpicsValue::String("1".into()));
        assert!(
            inst.declared_overrides.is_empty(),
            "a field the record serves via get_field must not enter the override map"
        );
    }
}

#[cfg(test)]
mod pact_exit_tests {
    use super::*;
    use crate::server::records::ai::AiRecord;

    fn instance() -> RecordInstance {
        RecordInstance::new("PACT:REC".to_string(), AiRecord::new(0.0))
    }

    /// The two boundary values of the bit `leave_pact` mints. It is minted
    /// under the `&mut self` the caller already holds, which is what lets
    /// `PvDatabase::apply_pact_exit` take no record lock and so be safe to
    /// call from a `Drop` that still has a `rec.write()` alive in scope.
    #[test]
    fn leave_pact_reports_an_empty_restart_queue_as_nothing_to_do() {
        let mut inst = instance();
        inst.enter_pact();
        assert!(!inst.leave_pact().restart_pending());
    }

    #[test]
    fn leave_pact_reports_a_queued_notify_so_the_tail_drains_it() {
        let mut inst = instance();
        inst.enter_pact();
        let (tx, _rx) = crate::runtime::sync::oneshot::channel();
        inst.queue_notify_put(DeferredNotify::Process { completion: tx });
        assert!(inst.leave_pact().restart_pending());
    }
}

#[cfg(test)]
mod declaration_gate_tests {
    use super::*;
    use crate::server::records::{bi::BiRecord, calc::CalcRecord, histogram::HistogramRecord};

    fn inst(name: &str, record: Box<dyn Record>) -> RecordInstance {
        RecordInstance::new_boxed(name.to_string(), record)
    }

    /// The boundary is DECLARED / NOT DECLARED, one case each way per
    /// storage that this port keeps for every record but C keeps per record
    /// type. Every expectation measured on `softIoc` R7.0.10-146 with
    /// `record(calc,"C:GOOD")`, `record(bi,"B:ONE")`,
    /// `record(histogram,"H:ONE")`:
    ///
    /// ```text
    /// dbgf C:GOOD.OUT      PV 'C:GOOD.OUT' not found
    /// dbgf C:GOOD.INP      PV 'C:GOOD.INP' not found
    /// dbgf C:GOOD.SSCN     PV 'C:GOOD.SSCN' not found
    /// dbgf C:GOOD.OLDSIMM  PV 'C:GOOD.OLDSIMM' not found
    /// dbgf C:GOOD.NOSUCH   PV 'C:GOOD.NOSUCH' not found
    /// dbgf C:GOOD.RTYP     DBF_STRING: "calc"
    /// dbgf C:GOOD.NAME     DBF_STRING: "C:GOOD"
    /// dbgf B:ONE.INP       DBF_STRING: ""
    /// dbgf B:ONE.OUT       PV 'B:ONE.OUT' not found
    /// dbgf B:ONE.HIHI      PV 'B:ONE.HIHI' not found
    /// dbgf H:ONE.INP       PV 'H:ONE.INP' not found
    /// ```
    #[test]
    fn a_field_resolves_exactly_where_the_record_type_declares_it() {
        let calc = inst("C:GOOD", Box::new(CalcRecord::default()));
        for undeclared in ["OUT", "INP", "SSCN", "OLDSIMM", "NOSUCH"] {
            assert_eq!(calc.resolve_field(undeclared), None, "calc.{undeclared}");
        }
        // Undeclared, but C's `dbNameToAddr` falls through to the record
        // type's attributes for it.
        assert_eq!(
            calc.resolve_field("RTYP"),
            Some(EpicsValue::String("calc".into()))
        );
        // Declared by dbCommon, so it stays readable.
        assert_eq!(
            calc.resolve_field("NAME"),
            Some(EpicsValue::String("C:GOOD".into()))
        );
        assert!(calc.resolve_field("CALC").is_some());

        // The same storage, on a record type that DOES declare INP and does
        // not declare OUT or the analog-alarm ladder.
        let bi = inst("B:ONE", Box::new(BiRecord::default()));
        assert_eq!(bi.resolve_field("INP"), Some(EpicsValue::String("".into())));
        assert_eq!(bi.resolve_field("OUT"), None);
        assert_eq!(bi.resolve_field("HIHI"), None);

        // `histogramRecord.dbd` declares SVL, not INP — the case
        // `Record::declares_inp_link` was written for, now answered by the
        // declaration itself.
        let histogram = inst("H:ONE", Box::new(HistogramRecord::default()));
        assert_eq!(histogram.resolve_field("INP"), None);
        assert!(histogram.resolve_field("SVL").is_some());
    }

    /// The channel-existence side must agree with the read side, or a client
    /// gets a SEARCH answered and a CREATE refused (or worse, the reverse).
    /// `resolve_string_view_field` is the `$` long-string route to the same
    /// funnel.
    #[test]
    fn the_long_string_view_is_gated_by_the_same_declaration() {
        let calc = inst("C:GOOD", Box::new(CalcRecord::default()));
        assert_eq!(calc.resolve_string_view_field("OUT"), None);
        assert!(calc.resolve_string_view_field("CALC").is_some());
    }
}

#[cfg(test)]
mod link_field_rendering_tests {
    use super::render_link_field;

    /// One case per boundary of C's `dbGetString` link switch
    /// (`dbStaticLib.c:1906-2050`), not one per scenario: the modifier chain has
    /// a defaulted arm, and the three field types mask it differently, so the
    /// cases that matter are the mask edges rather than a walk of realistic
    /// links.
    #[test]
    fn a_link_field_renders_with_cs_parsed_modifiers() {
        use crate::types::DbfLinkClass::{FwdLink, InLink, OutLink};
        for (class, text, want) in [
            // An input link's absent modifiers are C's defaults, not absences.
            (InLink, "L:B", "L:B NPP NMS"),
            (InLink, "L:B MS", "L:B NPP MS"),
            (InLink, "L:B PP MS", "L:B PP MS"),
            (InLink, "L:B MSI", "L:B NPP MSI"),
            (InLink, "L:B MSS", "L:B NPP MSS"),
            // The process class is one assignment down C's chain, so ` CA`
            // appears only when no PP/CP/CPP won it.
            (InLink, "L:B CA", "L:B CA NMS"),
            (InLink, "L:B CP", "L:B CP NMS"),
            (InLink, "L:B CPP", "L:B CPP NMS"),
            (InLink, "L:B CP CA", "L:B CA NMS"),
            (InLink, "L:B CP NPP", "L:B NPP NMS"),
            // The target is the slice before the first space, verbatim: a
            // `.FIELD` survives where a rebuild through `channel_name` would
            // drop an explicit `.VAL`.
            (InLink, "L:B.SEVR MS", "L:B.SEVR NPP MS"),
            (InLink, "L:B.VAL", "L:B.VAL NPP NMS"),
            // `DBF_OUTLINK` masks CP/CPP off before the render sees it.
            (OutLink, "L:B", "L:B NPP NMS"),
            (OutLink, "L:B CPP MS", "L:B NPP MS"),
            // `DBF_FWDLINK` keeps only CA and prints no severity switch at all.
            (FwdLink, "L:B", "L:B"),
            (FwdLink, "L:B CA", "L:B CA"),
            (FwdLink, "L:B PP MS", "L:B"),
            // Everything that is not a PV link is its own text.
            (InLink, "12.5", "12.5"),
            (InLink, "", ""),
            (InLink, "[1, 2, 3]", "[1, 2, 3]"),
            (InLink, "@dev p1 p2", "@dev p1 p2"),
            (InLink, "{\"const\":1}", "{\"const\":1}"),
        ] {
            assert_eq!(render_link_field(class, text), want, "{class:?} {text:?}");
        }
    }
}
