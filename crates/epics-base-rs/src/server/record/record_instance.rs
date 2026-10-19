use std::borrow::Cow;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use crate::error::{CaError, CaResult};
use crate::server::database::LinkBacking;
use crate::server::event_queue::{EventReader, EventUser, PostBurst};
use crate::server::pv::{SubscribeRefused, Subscriber};
use crate::server::recgbl::EventMask;
use crate::server::snapshot::{
    ControlInfo, DisplayInfo, EnumInfo, EnumStringForm, PropertySupport,
};
use crate::types::c_parse::Converted;
use crate::types::{DbFieldType, EpicsValue, PvString, c_parse};

use super::alarm::{AlarmLimit, AlarmLimitField, AlarmSeverity, AnalogAlarmConfig};
use super::common_fields::{BkptFlag, CommonFields};
use super::link::{ParsedLink, parse_link_v2};
use super::menu_choices::MenuBound;
use super::record_trait::{
    AuxPostMask, CommonFieldPutResult, DeclaredFields, EnumSlots, FieldDeclaration, FieldDesc,
    InputLinkRequest, LinkReadAs, PostName, PostStage, PostedValue, ProcessSnapshot, Record,
    RecordProcessResult, Subroutine,
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
pub(crate) fn render_link_field(class: crate::types::DbfLinkClass, raw: &[u8]) -> PvString {
    use super::{LinkFieldType, LinkProcessPolicy};
    use crate::types::DbfLinkClass;

    let text = super::trim_link_text(raw);
    let ftype = LinkFieldType::for_class(class);
    // The parse already applied C's per-field-type modifier mask
    // (`dbStaticLib.c:2380-2391`), so a `DBF_FWDLINK` reaching the arm below
    // has had everything but `CA` cleared and cannot render a stale ` MS`.
    let (policy, ms, ca_class) = match super::parse_link_field(text, ftype) {
        ParsedLink::Db(link) => (link.policy, link.monitor_switch, false),
        ParsedLink::Ca(link) => (link.policy, link.monitor_switch, true),
        // The store holds the parsed bus numbers, so the text comes from
        // them and from nowhere else.
        ParsedLink::Hw(hw) => return hw.render().into(),
        _ => return text.into(),
    };
    let target = link_target_head(text);

    // A forward link prints its target and, alone among the modifiers, ` CA`
    // (`dbStaticLib.c:2034-2044`): no process class and no maximize-severity
    // switch, which is why C answers a bare `FLNK` with just the record name.
    if matches!(class, DbfLinkClass::FwdLink) {
        return if ca_class {
            [target, b" CA"].concat().into()
        } else {
            target.into()
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
    [
        target,
        pp.as_bytes(),
        b" ",
        monitor_switch_word(ms).as_bytes(),
    ]
    .concat()
    .into()
}

/// The slice of a link's text before its first space: C's `pv_link.pvname`.
fn link_target_head(text: &[u8]) -> &[u8] {
    text.split(|&b| b == b' ').next().unwrap_or(text)
}

/// [`render_link_field`] for a link `dbRemoveLink` removed, of type `ty`:
/// the remove clears `pvlMask` (`dbDbLink.c:138`, `dbCa.c:440`), so a
/// `PV_LINK` prints its target with ` NPP NMS`, or bare in a forward link
/// (`dbStaticLib.c:1935-1952`, `:2034-2044`); any other type keeps its text.
fn render_removed_link(
    class: crate::types::DbfLinkClass,
    ty: super::DbLinkType,
    raw: &[u8],
) -> PvString {
    let text = super::trim_link_text(raw);
    if ty != super::DbLinkType::PvLink {
        return text.into();
    }
    let target = link_target_head(text);
    if matches!(class, crate::types::DbfLinkClass::FwdLink) {
        target.into()
    } else {
        [target, b" NPP NMS"].concat().into()
    }
}

/// C `dbGetString`'s `PN_LINK` arm (`dbStaticLib.c:1930-1933`): the target
/// and the severity switch, nothing else — ` NMS` for the empty link a
/// `PN_LINK` devSup is left with, whose `pvname` is NULL.
fn render_pn_link(raw: &[u8]) -> PvString {
    let text = super::trim_link_text(raw);
    let ms = match super::parse_link_field(text, super::LinkFieldType::In) {
        ParsedLink::Db(link) => link.monitor_switch,
        ParsedLink::Ca(link) => link.monitor_switch,
        _ => super::MonitorSwitch::default(),
    };
    [
        link_target_head(text),
        b" ",
        monitor_switch_word(ms).as_bytes(),
    ]
    .concat()
    .into()
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
/// Read only through [`declared_no_mod`].
const DBCOMMON_NOMOD: &[&str] = &[
    "NAME", "STAT", "SEVR", "AMSG", "NSTA", "NSEV", "NAMSG", "ACKS", "ACKT", "LCNT", "PACT",
    "PUTF", "RPRO", "TIME", "UTAG",
];

/// The static sources of [`RecordInstance::is_no_mod`]: the dbCommon
/// `SPC_NOMOD` set, then the type's declaration `desc` of `field`.
fn declared_no_mod(field: &str, desc: Option<&'static FieldDesc>) -> bool {
    DBCOMMON_NOMOD.iter().any(|f| f.eq_ignore_ascii_case(field))
        || desc.is_some_and(|f| f.read_only)
}

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
///
/// Membership is BOTH counted and named, because the two questions have
/// different answers here and C only ever has to ask one of them. C keeps
/// no count at all — `ellCount(&pnotifyPvt->waitList)` (`dbNotify.c:460`)
/// IS its count, so its list answers "has the chain settled?" and "which
/// records must a cancel release?" at once. The port cannot merge them:
/// `pending` also carries contributions that own no record slot (the
/// initiator's own hold, and a `dbCaPutLinkCallback` awaiting its network
/// completion — `links.rs`), which C models as `state` rather than as list
/// entries. So `pending` answers settlement and the `joined` list answers
/// cancellation, each with one meaning on every path.
pub struct NotifyWaitSet {
    pending: AtomicUsize,
    /// C `notifyPvt::waitList` (`dbNotify.c:73`) — every record that took
    /// this set into its `notify` slot, so a cancel can sweep them the way
    /// `dbNotifyCancel` walks the wait list (`:428-430`).
    ///
    /// Append-only, and deliberately so: C deletes a member on completion
    /// only because `ellCount` is its settlement count, which `pending`
    /// already is here. Keeping a completed member listed costs a slot
    /// re-check and cannot lose one, whereas removing on completion would
    /// make the list the second thing that has to be right for a cancel to
    /// reach a record.
    ///
    /// Names, not handles: the sweep needs the record's write lock anyway,
    /// and the authority on membership is the record's own slot — a stale
    /// name simply fails the `Arc::ptr_eq` and is skipped. That makes
    /// over-listing harmless and under-listing the only failure, which is
    /// what the append-only rule then makes unrepresentable.
    ///
    /// Written in exactly one place, [`RecordInstance::take_notify_slot`],
    /// which is also the only writer of the slot itself — so "every record
    /// holding this set is named here or as [`Self::entry`]" holds by
    /// construction. The entry record is never repeated here: it is named
    /// already, and a WRITE_NOTIFY then pays no second copy of its name.
    joined: StdMutex<Vec<crate::types::RecordName>>,
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
    /// which passes its own record, and the other path into the slot
    /// ([`RecordInstance::join_put_notify`]) clones a set it did not make —
    /// so "the entry names the record whose slot minted it" holds by
    /// construction rather than by a check.
    ///
    /// It is half the membership list: [`Self::joined`] names every other
    /// member, and [`Self::joined_records`] yields this one first. It is also
    /// the member `dbNotifyDump` prints a block for (`dbNotify.c:659-660`).
    ///
    /// [`PvDatabase::new_put_notify`]: crate::server::database::PvDatabase::new_put_notify
    entry: Option<crate::types::RecordName>,
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
            joined: StdMutex::new(Vec::new()),
            tx: StdMutex::new(Some(tx)),
            entry: None,
        })
    }

    /// C `dbProcessNotify` (`dbNotify.c:196-270`): the put named `record`, so
    /// `dbChannelRecord(ppn->chan)` is `record` and that is the one record
    /// `dbNotifyDump` prints a block for.
    pub(crate) fn for_entry_record(
        record: &crate::types::RecordName,
        tx: crate::runtime::sync::oneshot::Sender<()>,
    ) -> Arc<Self> {
        Arc::new(Self {
            pending: AtomicUsize::new(1),
            joined: StdMutex::new(Vec::new()),
            tx: StdMutex::new(Some(tx)),
            entry: Some(record.clone()),
        })
    }

    /// The record this notify was issued against, or `None` for a set with
    /// no `processNotify` behind it. See [`Self::entry`].
    pub(crate) fn entry_record(&self) -> Option<&crate::types::RecordName> {
        self.entry.as_ref()
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

    /// A member was destroyed and will never leave: end the set with the
    /// error a dropped completion gives its waiter. Takes the sender under
    /// the lock [`Self::leave`] fires it under, so the set ends exactly once.
    pub(crate) fn abandon(&self) {
        drop(self.tx.lock().unwrap().take());
    }

    /// True once every chain member has left (the completion has fired).
    /// Used by the put entry to decide synchronous ([`ProcessCompletion::Sync`])
    /// vs async-pending ([`ProcessCompletion::Async`]) completion.
    pub fn completed(&self) -> bool {
        self.pending.load(Ordering::Acquire) == 0
    }

    /// Record `name` took this set into its `notify` slot — C
    /// `ellSafeAdd(&pnotifyPvt->waitList, &precord->ppnr->waitNode)`
    /// (`dbNotify.c:227`/`:258`/`:498`).
    ///
    /// Private to this module and called from the one slot writer
    /// ([`RecordInstance::take_notify_slot`]), so joining the list and taking
    /// the slot are one act and cannot be done separately.
    fn record_joined(&self, name: &crate::types::RecordName) {
        if self.entry.as_ref() != Some(name) {
            self.joined.lock().unwrap().push(name.clone());
        }
    }

    /// Every record that has held this set — C's wait list as
    /// `dbNotifyCancel` enumerates it (`dbNotify.c:428`).
    ///
    /// A snapshot, because the sweep must take record write locks and cannot
    /// hold this mutex while it does. Growth after the snapshot is not a
    /// missed member: a record can only join a set that is still answerable,
    /// and this is read only of a set that is not.
    pub(crate) fn joined_records(&self) -> Vec<crate::types::RecordName> {
        let joined = self.joined.lock().unwrap();
        self.entry.iter().chain(joined.iter()).cloned().collect()
    }

    /// Nobody is left to answer: the completion never fired (the sender is
    /// still here) and the client that would have received it is gone.
    ///
    /// This is the condition C detects at client teardown —
    /// `rsrvFreePutNotify` sees `pNotify->busy` and calls `dbNotifyCancel`
    /// (`camessage.c:1630-1638`). A set that already fired holds no sender and
    /// is NOT unanswerable: it completed.
    pub(crate) fn is_unanswerable(&self) -> bool {
        self.tx
            .lock()
            .unwrap()
            .as_ref()
            .is_some_and(|tx| tx.is_closed())
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
    /// The field the client wrote, by its declared name, `$` kept.
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
    /// C `processRequest`: write nothing, process nothing; the callback
    /// fires once the record is free (`dbNotify.c:213-231`, `:263-270`).
    Wait {
        completion: crate::runtime::sync::oneshot::Sender<()>,
    },
    /// C `processGetRequest`: process the record, write nothing.
    Process {
        /// The client's completion channel, armed on the replayed process.
        completion: crate::runtime::sync::oneshot::Sender<()>,
    },
    /// C `processGetRequest` from a SoftCallback input dset, which restarts
    /// the record only if it is Passive (`dbNotify.c:247-249`) and is gone
    /// once the requester's `del_record` ran `dbNotifyCancel` (`:400-410`).
    ProcessGet {
        completion: crate::runtime::sync::oneshot::Sender<()>,
        requester: crate::server::database::TokenWatch,
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
/// `populate_enum_info` so subsequent `snapshot_for_field` calls can skip
/// rebuilding the metadata. The
/// cache is rebuilt whenever a metadata-class field is written
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
    /// `(hihi, high, low, lolo)` from [`RecordInstance::explicit_alarm_limits`]
    /// — the record's OWN bands, which C `get_alarm_double` reads as struct
    /// members (`calcRecord.c:205-221`). Record-level, not per-field: the
    /// function ignores the field and answers from `rtype` alone, so which
    /// FIELDS receive these is still [`RecordInstance::route_field_metadata`]'s
    /// decision — only the eight `resolve_field` name lookups behind the
    /// numbers move here, where a rebuild rather than a snapshot pays
    /// for them.
    pub alarm: (f64, f64, f64, f64),
}

/// [`RecordCold::metadata_cache`]: `None` until first built.
type MetadataCell = Arc<arc_swap::ArcSwapOption<MetadataSnapshot>>;

/// Does a write to this field make the record's metadata cache stale?
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
/// rebuild is owned by the DESC arm of `put_common_field`, the single
/// writer of `common.text.desc`. The `Q:form` info tag (`populate_display_info` ->
/// `display.form`) is an immutable load-time tag, not a runtime field, so it
/// needs no rebuild either.
fn is_metadata_cache_source(name: &str) -> bool {
    matches!(
        name,
        // `populate_display_info` — units/precision/display limits for the
        // analog, integer, array and motor arms.
        "EGU" | "PREC" | "HOPR" | "LOPR" | "HLM" | "LLM"
        // `populate_control_info` — the ao/longout/int64out drive limits.
        | "DRVH" | "DRVL"
        // `explicit_alarm_limits` — the four bands and the four severities
        // that gate them. C reads these as struct members on every
        // `get_alarm_double`, so nothing there has to be invalidated; here
        // they are cached, and a write to any of the eight is what makes the
        // cached answer wrong.
        | "HIHI" | "HIGH" | "LOW" | "LOLO"
        | "HHSV" | "HSV" | "LSV" | "LLSV"
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

/// Coerce a db-loaded `String` for a numeric/menu **common** field to that
/// field's canonical DBF type before [`RecordInstance::put_common_field`]
/// dispatches on it.
///
/// The db loader converts a record's own fields to their typed value
/// (`db_loader::apply_fields`), but a field absent from `field_list` is
/// pushed to the common-field path as a raw `EpicsValue::String` — it has no
/// `FieldDesc` to parse against. The numeric common-field arms in
/// `put_common_field` match only their typed variant, so without this step a
/// `.db` `field(PHAS, "1")`,
/// `field(PRIO, "HIGH")`, `field(DISS, "MAJOR")`, `field(DISA, "1")`, … is
/// silently dropped at IOC load. This step gives the String that typed value,
/// numeric *and* menu-label forms alike, so the arm receives what it expects.
///
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
        // value-coercion owner; a value already `dbf` moves through.
        return Ok(Converted::Stored(value.into_type(dbf)));
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
        c_parse::Converted::Stored(parsed) => Converted::Stored(parsed.into_type(dbf)),
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

/// This cycle's forward link, as one copied handle: no arm holds a share of
/// a link or a record, so nothing is cloned for a dispatch and nothing of the
/// source's link is borrowed across the target's processing, which may put
/// that link.
#[derive(Clone, Copy)]
pub(crate) enum ForwardTarget {
    /// Nothing to forward: the record vetoed its own FLNK this cycle
    /// ([`Record::should_fire_forward_link`]), or `FLNK` is unset / constant /
    /// hardware — every kind C's `dbScanFwdLink` walks past.
    None,
    /// A local database record, C `dbScanFwdLink` → `dbScanPassive` →
    /// `processTarget`: the target `dbDbInitLink` bound
    /// ([`DbLink::forward`](crate::server::record::DbLink::forward)), the
    /// source's PUTF, and whether it has a put-notify wait-set, which the
    /// dispatch reads as C's `processTarget` reads `psrc->ppn`.
    Db {
        target: crate::server::database::RecordIdx,
        putf: bool,
        notify: bool,
    },
    /// An external `pva://` / `ca://` PV, C `dbScanFwdLink` → the link set's
    /// `scanForward` (pvxs `pvaScanForward`) — a process-only trigger. The
    /// dispatch reads FLNK again, as C reads `plink` there.
    External,
}

/// What C's `readValue`/`writeValue` reaches as struct members — `prec->sims`,
/// `->sdly` — and which stage of the cycle C put the SIOL read in. `siml` and
/// `siol` are links, read through their slots ([`RecordInstance::sim_link`]).
///
/// C names these fields at compile time; this port resolved all four BY NAME
/// on every cycle of every record whose `.dbd` declares SIMM (`ai`, `bi`,
/// `longin`, `mbbi`, `waveform`, ...). Each resolution upper-cased the name
/// into a fresh `String` and then scanned the record type's field table and
/// `dbCommon`'s — measured as the single largest cost of an `ai` cycle, more
/// than the record body. The declaration is fixed at the type, so it is read
/// here once per record instance and the cycle reads through it.
#[derive(Clone, Copy, Debug)]
pub(crate) struct SimmBlock {
    pub(crate) sims: Option<&'static FieldDesc>,
    pub(crate) sdly: Option<&'static FieldDesc>,
    /// C put this type's SIOL read in the INPUT stage: `readValue` reads SIOL
    /// IN (`aiRecord.c`, `mbbiRecord.c:388-394`, `waveformRecord.c:351`,
    /// `histogramRecord.c:384`, `mcaRecord.c:1097`, and `aai` through
    /// `devAaiSoft.c:89`), where `writeValue` writes VAL OUT to it. Answering
    /// this from the record TYPE NAME meant allocating that name as a `String`
    /// once per cycle to match against it.
    pub(crate) is_input: bool,
    /// [`Record::aborts_on_failed_siml_read`].
    pub(crate) aborts_on_failed_siml_read: bool,
    /// [`Record::raises_simm_after_read`].
    pub(crate) raises_simm_after_read: bool,
}

impl SimmBlock {
    fn of(record: &dyn Record) -> Self {
        let rtype = record.record_type();
        SimmBlock {
            sims: field_desc_of(record, "SIMS"),
            sdly: field_desc_of(record, "SDLY"),
            is_input: record.simulation_substitutes_input_stage()
                || matches!(
                    rtype,
                    "ai" | "bi"
                        | "mbbi"
                        | "mbbiDirect"
                        | "longin"
                        | "int64in"
                        | "stringin"
                        | "lsi"
                        | "event"
                        | "waveform"
                        | "histogram"
                        | "aai"
                        | "mca"
                ),
            aborts_on_failed_siml_read: record.aborts_on_failed_siml_read(),
            raises_simm_after_read: record.raises_simm_after_read(),
        }
    }
}

/// C's "input dset returns 2" for a soft input: the DTYP is Plain or Async
/// (never Raw), the record is not an output, and its type skips the convert.
/// A macro, not a fn: the process cycle's `run_process_frame` grew 10
/// instr/cycle on ai and ao when this was a call, whatever the inlining.
/// The cycle (plan answer) and
/// [`RecordInstance::soft_input_skips_convert`] share this one test.
macro_rules! input_dset_skips_convert {
    ($dtyp_returns_2:expr, $is_output:expr, $record_skips_convert:expr) => {
        $dtyp_returns_2 && !$is_output && $record_skips_convert
    };
}
pub(crate) use input_dset_skips_convert;

fn menu_index_of(record: &dyn Record, field: &str) -> i16 {
    record
        .get_field(field)
        .and_then(|v| v.to_menu_index())
        .unwrap_or(0)
}

impl ProcessPlan {
    /// The type-static half of [`CyclePlan::ConstSoftIn`]: the `ai`/`bi`
    /// shape with no simulation block, which the arm's per-cycle SIMM test
    /// makes equivalent to the declared one.
    pub(crate) const PLAIN_INPUT: ProcessPlan = ProcessPlan {
        dset_can_refuse: true,
        simm: None,
        sel_nvl: false,
        string_input: false,
        multi_output_dispatch: false,
        reads_sell: false,
        posts_software_event: false,
        resolves_subroutine_from_link: false,
        runs_subroutine: false,
        substitutes_input_stage_when_simulating: false,
        redecides_after_output: false,
        acts_before_process: false,
        hooks: crate::server::record::CycleHooks::NONE,
        input_fetch_policy: crate::server::record::InputFetchPolicy::ReadAll,
        constants_deliver_at_process: false,
        multi_input_is_db_get_link: true,
        multi_inputs_read_native: true,
        dispatches_generic_multi_output: false,
        has_multi_input_links: false,
        narrows_input_links: false,
        output_stage: false,
        fetches_dol_closed_loop: false,
        soft_channel_skips_convert: true,
        skips_timestamp_when_undefined: false,
        skips_forward_convert_when_undefined: false,
        posts_output_event: false,
        reads_links_at_output: false,
        stamp_point: crate::server::record::StampPoint::BeforeAlarms,
        stamps_before_alarms: true,
        reads_tsel_out_of_line: false,
        works_before_monitor: false,
        stamps_before_siml_read: false,
        clears_udf: true,
        soft_read_clears_udf: true,
        can_device_write: false,
    };

    /// Whether this type's cycle is [`Self::PLAIN_INPUT`] once the simulation
    /// block is set aside. Compared field by field so a field added to the
    /// struct fails to compile here rather than silently defaulting.
    pub(crate) fn is_plain_input(&self) -> bool {
        let ProcessPlan {
            dset_can_refuse,
            simm: _,
            sel_nvl,
            string_input,
            multi_output_dispatch,
            reads_sell,
            posts_software_event,
            resolves_subroutine_from_link,
            runs_subroutine,
            substitutes_input_stage_when_simulating,
            redecides_after_output,
            acts_before_process,
            hooks,
            input_fetch_policy,
            constants_deliver_at_process,
            multi_input_is_db_get_link,
            multi_inputs_read_native,
            dispatches_generic_multi_output,
            has_multi_input_links,
            narrows_input_links,
            output_stage,
            fetches_dol_closed_loop,
            soft_channel_skips_convert,
            skips_timestamp_when_undefined,
            skips_forward_convert_when_undefined,
            posts_output_event,
            reads_links_at_output,
            stamp_point,
            stamps_before_alarms,
            reads_tsel_out_of_line,
            works_before_monitor,
            stamps_before_siml_read,
            clears_udf,
            soft_read_clears_udf,
            can_device_write,
        } = *self;
        let p = &Self::PLAIN_INPUT;
        dset_can_refuse == p.dset_can_refuse
            && sel_nvl == p.sel_nvl
            && string_input == p.string_input
            && multi_output_dispatch == p.multi_output_dispatch
            && reads_sell == p.reads_sell
            && posts_software_event == p.posts_software_event
            && resolves_subroutine_from_link == p.resolves_subroutine_from_link
            && runs_subroutine == p.runs_subroutine
            && substitutes_input_stage_when_simulating == p.substitutes_input_stage_when_simulating
            && redecides_after_output == p.redecides_after_output
            && acts_before_process == p.acts_before_process
            && hooks == p.hooks
            && input_fetch_policy == p.input_fetch_policy
            && constants_deliver_at_process == p.constants_deliver_at_process
            && multi_input_is_db_get_link == p.multi_input_is_db_get_link
            && multi_inputs_read_native == p.multi_inputs_read_native
            && dispatches_generic_multi_output == p.dispatches_generic_multi_output
            && has_multi_input_links == p.has_multi_input_links
            && narrows_input_links == p.narrows_input_links
            && output_stage == p.output_stage
            && fetches_dol_closed_loop == p.fetches_dol_closed_loop
            && soft_channel_skips_convert == p.soft_channel_skips_convert
            && skips_timestamp_when_undefined == p.skips_timestamp_when_undefined
            && skips_forward_convert_when_undefined == p.skips_forward_convert_when_undefined
            && posts_output_event == p.posts_output_event
            && reads_links_at_output == p.reads_links_at_output
            && stamp_point == p.stamp_point
            && stamps_before_alarms == p.stamps_before_alarms
            && reads_tsel_out_of_line == p.reads_tsel_out_of_line
            && works_before_monitor == p.works_before_monitor
            && stamps_before_siml_read == p.stamps_before_siml_read
            && clears_udf == p.clears_udf
            && soft_read_clears_udf == p.soft_read_clears_udf
            && can_device_write == p.can_device_write
    }

    /// The answers, asked of the record type once. Pure in the record's
    /// type: every input is a `Record` method or a table keyed on its name,
    /// so the plan is the same whenever it is built.
    pub(crate) fn of(record: &dyn Record) -> Self {
        let rtype = record.record_type();
        let simm = field_desc_of(record, "SIMM")
            .is_some()
            .then(|| SimmBlock::of(record));
        let output_stage = record.can_device_write()
            || field_desc_of(record, "OUT").is_some()
            || field_desc_of(record, "OEVT").is_some()
            || crate::server::database::multi_output_dispatch_owned(rtype)
            || record.declares_multi_output_links()
            // An INPUT type's simulation substitutes its read and
            // returns before the body; only an output type's SIOL write
            // reaches this stage.
            || simm.is_some_and(|block| !block.is_input);
        ProcessPlan {
            dset_can_refuse: crate::server::recgbl::dev_sup_process_refusal(rtype).is_some(),
            simm,
            sel_nvl: rtype == "sel",
            string_input: !record.string_input_links().is_empty(),
            multi_output_dispatch: crate::server::database::multi_output_dispatch_owned(rtype),
            reads_sell: crate::server::database::reads_sell(rtype),
            posts_software_event: crate::server::database::posts_software_event(rtype),
            resolves_subroutine_from_link: crate::server::database::resolves_subroutine_from_link(
                rtype,
            ),
            runs_subroutine: SubroutineKind::of(rtype).is_some(),
            // Asked of the record itself, not of a table keyed on `rtype`: the
            // type that implements the step is the one that answers whether it
            // has one, so the two cannot drift into different files.
            substitutes_input_stage_when_simulating: record.simulation_substitutes_input_stage(),
            redecides_after_output: record.redecides_after_output(),
            acts_before_process: record.acts_before_process(),
            hooks: record.cycle_hooks(),
            input_fetch_policy: record.input_fetch_policy(),
            constants_deliver_at_process: record.constant_inputs_deliver_at_process(),
            multi_input_is_db_get_link: record.multi_input_fetch_is_db_get_link(),
            multi_inputs_read_native: record.input_link_answers_fixed_at_type()
                && record.multi_input_links().iter().all(|(lf, _)| {
                    record.input_link_request(lf) == InputLinkRequest::As(LinkReadAs::Native)
                        && !record.input_link_failure_is_inert(lf)
                }),
            dispatches_generic_multi_output: !crate::server::database::multi_output_dispatch_owned(
                rtype,
            ) && record.declares_multi_output_links(),
            has_multi_input_links: !record.multi_input_links().is_empty(),
            narrows_input_links: record.narrows_input_links(),
            output_stage,
            fetches_dol_closed_loop: record.fetches_dol_closed_loop(),
            soft_channel_skips_convert: record.soft_channel_skips_convert(),
            skips_timestamp_when_undefined: record.skips_timestamp_when_undefined(),
            skips_forward_convert_when_undefined: record.skips_forward_convert_when_undefined(),
            posts_output_event: field_desc_of(record, "OEVT").is_some(),
            reads_links_at_output: record.declares_output_time_input_links(),
            stamp_point: record.stamp_point(),
            stamps_before_alarms: record.stamp_point()
                == crate::server::record::StampPoint::BeforeAlarms,
            reads_tsel_out_of_line: record.cycle_hooks().withholds_cycle_stamp()
                || record.stamp_point().is_late(),
            works_before_monitor: record
                .cycle_hooks()
                .has(crate::server::record::CycleHooks::HAND_POSTED)
                || record.stamp_point() == crate::server::record::StampPoint::AfterLinkWrites,
            stamps_before_siml_read: output_stage && !record.stamps_after_write(),
            clears_udf: record.clears_udf(),
            soft_read_clears_udf: record.soft_read_clears_udf(),
            can_device_write: record.can_device_write(),
        }
    }
}

/// C's `if (prec->udf) recGblSetSevr(prec, UDF_ALARM, prec->udfs);` line, as
/// one record type writes it — whether the type has the line at all, whether
/// it tests `udf == TRUE` instead of truthiness, the severity it substitutes
/// for `UDFS`, and the message it attaches. Four `&dyn Record` calls, once per
/// cycle per record, for four answers the type fixed at construction.
#[derive(Clone, Copy, Debug)]
pub(crate) struct UdfAlarm {
    pub(crate) exact_one: bool,
    pub(crate) severity: Option<crate::server::record::AlarmSeverity>,
    pub(crate) message: &'static str,
}

mod link_cache {
    use crate::server::builtin_devices::soft_callback::AddRecordRefusal;
    use crate::server::database::ProcessTargetGate;
    use crate::server::device_support::SoftDtyp;
    use crate::server::recgbl::simm::has_const_lset;
    use crate::server::record::{DbLinkType, LinkFieldType, MonitorSwitch, ParsedLink};
    use crate::types::PvString;
    use std::sync::{Arc, LazyLock};

    /// A parsed `dbCommon` link, readable as the [`ParsedLink`] it holds and
    /// replaceable only from `record_instance` — through
    /// `RecordInstance::install_link_cache`, which stamps the `CyclePlan`.
    /// The parse is shared so a swap under the hold is two pointer moves:
    /// the new `Arc` is built before the hold and the old one is dropped
    /// after it.
    ///
    /// `out` is what the link keeps beyond its parse: nothing, except OUT's
    /// [`OutTargetSlot`], so the four other entries stay 16 bytes and share
    /// a cache line.
    #[derive(Debug)]
    pub struct LinkCache<T = ()> {
        facts: LinkFacts,
        parse: Arc<ParsedLink>,
        out: T,
    }

    /// The OUT entry, which also keeps the frame's copy of its lent target.
    pub type OutLinkCache = LinkCache<OutTargetSlot>;

    /// [`OutLinkCache`]'s target copy, emptied by every swap of the parse.
    #[derive(Debug, Default)]
    pub struct OutTargetSlot(Option<OutTarget>);

    // SAFETY: the `OutTarget` is dereferenced only by the frame of the set
    // that owns this record, under that set's hold; see `OutTarget`.
    unsafe impl Send for OutTargetSlot {}
    unsafe impl Sync for OutTargetSlot {}

    /// What the process cycle asks of the link on every pass, copied from
    /// the parse whenever one is installed so the cycle answers from the
    /// entry without following the `Arc` (spec-R2 §10: resolved at attach).
    #[derive(Debug, Clone, Copy)]
    struct LinkFacts {
        constant: bool,
        writable_out: bool,
        gate: Option<ProcessTargetGate>,
        switch: MonitorSwitch,
    }

    impl LinkFacts {
        fn of(link: &ParsedLink) -> Self {
            let gate = match link {
                ParsedLink::Db(db) => ProcessTargetGate::of_put(db),
                _ => None,
            };
            Self {
                constant: crate::server::recgbl::simm::is_constant(link),
                writable_out: link.is_writable_out_link(),
                gate,
                switch: link.monitor_switch().unwrap_or_default(),
            }
        }
    }

    /// An OUT link's local target as the frame writes it, C's `dbAddr` of
    /// `dbDbPutValue`: the target record's cell, its put address, and the
    /// name revision both were resolved at.
    ///
    /// `rec` borrows the cell the link's lent entry (`DbLink::lend_out_addr`)
    /// keeps alive. That entry is emptied only by
    /// `RecordInstance::release_link_addrs`, under `&mut` and a full acquire
    /// of the owning set, or by the `DbLink`'s drop after `LinkCache::swap`
    /// under the hold, and both of those clear this copy first; so a frame
    /// holding the owning set, at a matching revision, sees a live cell. Not
    /// `Send`: a copy never leaves the frame.
    #[derive(Clone, Copy)]
    pub(crate) struct OutTarget {
        rec: std::ptr::NonNull<super::RecordCell>,
        put: super::PutAddr,
        revision: u64,
    }

    impl std::fmt::Debug for OutTarget {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.debug_struct("OutTarget")
                .field("rec", &self.rec)
                .field("revision", &self.revision)
                .finish_non_exhaustive()
        }
    }

    impl OutTarget {
        /// `rec` is the lent entry's cell, so the pointer is an `Arc`'s.
        pub(crate) fn new(
            rec: &Arc<super::RecordCell>,
            put: super::PutAddr,
            revision: u64,
        ) -> Self {
            Self {
                rec: std::ptr::NonNull::from(&**rec),
                put,
                revision,
            }
        }

        /// The target cell, for the frame of `owner`'s set at a revision
        /// equal to [`Self::revision`]: no refcount, nothing to drop.
        ///
        /// # Safety
        ///
        /// The copy is the one `owner`'s OUT [`LinkCache`] held when the
        /// frame read it under `held`, and the frame checked its revision
        /// against `name_revision()` then; see [`OutTarget`].
        pub(crate) unsafe fn lend(
            &self,
            held: &crate::server::database::SetGuard,
            owner: &super::RecordCell,
        ) -> std::mem::ManuallyDrop<Arc<super::RecordCell>> {
            debug_assert!(
                held.holds(&owner.lock_record),
                "an OUT target lent outside its owner's set"
            );
            // SAFETY: `new` took the pointer from an `Arc`, and the caller's
            // contract keeps that `Arc` alive; `ManuallyDrop` never releases
            // the count this does not hold.
            std::mem::ManuallyDrop::new(unsafe { Arc::from_raw(self.rec.as_ptr()) })
        }

        pub(crate) fn put(&self) -> super::PutAddr {
            self.put
        }

        #[cfg(test)]
        pub(crate) fn is_cell(&self, rec: &super::RecordCell) -> bool {
            std::ptr::eq(self.rec.as_ptr(), rec)
        }

        pub(crate) fn revision(&self) -> u64 {
            self.revision
        }
    }

    impl<T: Default> LinkCache<T> {
        pub(super) fn none() -> Self {
            let parse = SHARED[NONE].clone();
            LinkCache {
                facts: LinkFacts::of(&parse),
                parse,
                out: T::default(),
            }
        }

        pub fn link(&self) -> &ParsedLink {
            &self.parse
        }

        /// The parse itself, for a writer that must keep it past a swap.
        pub(crate) fn shared(&self) -> &Arc<ParsedLink> {
            &self.parse
        }

        /// C `dbLinkIsConstant` (`dbLink.c:215-225`).
        pub fn is_constant(&self) -> bool {
            self.facts.constant
        }

        /// [`ParsedLink::is_writable_out_link`].
        pub fn is_writable_out_link(&self) -> bool {
            self.facts.writable_out
        }

        /// [`ProcessTargetGate::of_put`] of a DB link.
        pub(crate) fn process_gate(&self) -> Option<ProcessTargetGate> {
            self.facts.gate
        }

        /// [`ParsedLink::monitor_switch`], `NMS` for a link without one.
        pub(crate) fn monitor_switch(&self) -> MonitorSwitch {
            self.facts.switch
        }

        /// The swap empties `out` with the facts: the old parse, and the
        /// entry it lent, leave with `link`.
        pub(super) fn swap(&mut self, link: &mut Arc<ParsedLink>) {
            std::mem::swap(&mut self.parse, link);
            self.facts = LinkFacts::of(&self.parse);
            self.out = T::default();
        }
    }

    impl OutLinkCache {
        pub(crate) fn out_target(&self) -> Option<OutTarget> {
            self.out.0
        }

        /// Store the frame's copy of the lent OUT target, under the write
        /// guard, from the entry `DbLink::lend_out_addr` lent.
        pub(crate) fn set_out_target(&mut self, target: Option<OutTarget>) {
            self.out.0 = target;
        }
    }

    /// A `dbCommon` link's new text and parse, made before the record's hold
    /// — C's `dbPutFieldLink` parses ahead of `dbScanLock` — so the hold
    /// only swaps them in, and the old pair leaves in this value to be
    /// dropped after the hold.
    pub(crate) struct PreparedCommonLink {
        pub(super) field: &'static str,
        ftype: LinkFieldType,
        pub(super) text: PvString,
        pub(super) parsed: Arc<ParsedLink>,
        /// `dbParseLink`'s answer, which [`Self::relink`] leaves alone.
        as_parsed: Arc<ParsedLink>,
        /// The new dset of a device-link put, prepared before the hold by
        /// [`Self::ask_add_record`].
        pub(super) device: Option<PreparedDevice>,
        /// What a soft-channel `add_record` refused, for the caller to
        /// report once the hold is down.
        pub(crate) add_record_refusal: Option<AddRecordRefusal>,
    }

    /// C `dbPutFieldLink`'s `new_dset` and the `add_record` answer for it
    /// (`dbAccess.c:1128-1131`, `:1196`), for the DTYP read when it was
    /// prepared.
    pub(crate) struct PreparedDevice {
        pub(super) dtyp: crate::server::device_support::DtypName,
        pub(super) dset: NewDset,
        /// The object the swap took off the record, dropped with the prepared
        /// link once the hold is down.
        pub(super) retired: Option<Box<dyn crate::server::device_support::DeviceSupport>>,
        /// The `dpvt` the new dset's `add_record` made, and the one the swap
        /// took off the record (`dbAccess.c:1192`).
        pub(super) dpvt: crate::server::device_support::Dpvt,
        pub(super) retired_dpvt: crate::server::device_support::Dpvt,
        /// C's saved `scan` was I/O Intr and `scanDelete` ran
        /// (`dbAccess.c:1155-1158`): the put still owes its scan exit.
        pub(super) left_io_intr: bool,
        /// The scan-list move that exit left, for the database to file.
        pub(super) scan_moved: crate::server::record::CommonFieldPutResult,
    }

    pub(crate) enum NewDset {
        /// No device object: a soft channel, or a DTYP no factory serves.
        Absent,
        /// The bound object stays: its dset extends with `devSoft_DSXT`,
        /// whose `add_record` binds nothing.
        Keep,
        /// A fresh object for the new link, whose `add_record` the rebind
        /// runs under the hold; one that refused is left here to be freed
        /// after it.
        Bound(Box<dyn crate::server::device_support::DeviceSupport>),
        /// `dev<T>SoftCallback`'s `add_record` refused; its line is printed
        /// after the hold.
        SoftRefused(AddRecordRefusal),
    }

    impl PreparedCommonLink {
        /// `Some` when `field` is one of the `dbCommon` link fields (and
        /// swait's `OUTN`) and `value` a string.
        pub(crate) fn for_put(field: &str, value: &crate::types::EpicsValue) -> Option<Self> {
            let crate::types::EpicsValue::String(text) = value else {
                return None;
            };
            #[cfg(test)]
            super::put_gate_probe::parsed();
            let (field, ftype) = [
                ("FLNK", LinkFieldType::Fwd),
                ("INP", LinkFieldType::In),
                ("OUT", LinkFieldType::Out),
                ("TSEL", LinkFieldType::In),
                ("SDIS", LinkFieldType::In),
                ("OUTN", LinkFieldType::Out),
            ]
            .into_iter()
            .find(|(f, _)| f.eq_ignore_ascii_case(field))?;
            let text = text.clone();
            let parsed = parse(text.as_bytes(), ftype);
            Some(PreparedCommonLink {
                field,
                ftype,
                as_parsed: Arc::clone(&parsed),
                parsed,
                text,
                device: None,
                add_record_refusal: None,
            })
        }

        /// The parse before [`Self::relink`]: what C `dbPutFieldLink`
        /// creates its channel from, ahead of `dbAddLink`.
        pub(crate) fn as_parsed(&self) -> &ParsedLink {
            &self.as_parsed
        }

        /// C `dbAddLink`'s view of the parse: `relink` answers what the link
        /// becomes in this IOC, `None` for unchanged.
        pub(crate) fn relink(
            &mut self,
            relink: impl FnOnce(&ParsedLink, LinkFieldType) -> Option<ParsedLink>,
        ) {
            if let Some(parsed) = relink(&self.parsed, self.ftype) {
                self.parsed = Arc::new(parsed);
            }
        }

        pub(crate) fn field(&self) -> &'static str {
            self.field
        }

        /// C `dbPutFieldLink`'s `new_dset` and `new_dsxt->add_record(precord)`
        /// (`dbAccess.c:1128`, `:1196`) for an INP/OUT put to `rec`, decided
        /// here because both look outside the record: the DTYP and record
        /// type are read under a short read hold, the factory and the target
        /// lookup run after it.
        pub(crate) fn ask_add_record(
            &mut self,
            db: &crate::server::database::PvDatabase,
            rec: &super::RecordCell,
        ) {
            if !matches!(self.field, "INP" | "OUT") {
                return;
            }
            let at = super::PutAddr::resolve_in(rec, self.field);
            let (record_type, dtyp, bound_here, no_mod) = {
                let inst = rec.read();
                if !inst.has_device_list() {
                    return;
                }
                let dtyp = inst.common.dtyp.shared_name();
                let bound_here = inst.device.as_ref().is_some_and(|d| d.dtyp() == &*dtyp);
                let no_mod = inst.is_no_mod_at(self.field, &at);
                (inst.record.record_type(), dtyp, bound_here, no_mod)
            };
            let mut dpvt = crate::server::device_support::Dpvt::Null;
            let row = super::super::dev_sup_of(record_type, &dtyp);
            let dset = if no_mod {
                // `dbPutSpecial(paddr, 0)` refuses it under the hold
                // (`dbAccess.c:1176`), before `add_record` is reached.
                NewDset::Keep
            } else if row.is_some_and(|row| !row.has_dsxt()) {
                // Refused under the hold before `add_record` is reached
                // ([`super::RecordInstance::gate_device_link_put`]).
                NewDset::Absent
            } else if let Some(class) =
                crate::server::device_support::served_soft(record_type, &dtyp)
            {
                let added = if self.field == "INP" && class == SoftDtyp::Async {
                    crate::server::builtin_devices::soft_callback::put_add_record(
                        db,
                        record_type,
                        &dtyp,
                        self.text.as_bytes(),
                    )
                } else {
                    Ok(crate::server::device_support::Dpvt::Null)
                };
                match added {
                    Ok(added) => {
                        dpvt = added;
                        NewDset::Absent
                    }
                    Err(refusal) => NewDset::SoftRefused(refusal),
                }
            } else if bound_here && !row.is_some_and(|row| row.dset.extends()) {
                NewDset::Keep
            } else {
                // A device address is text to its support; a name's bytes
                // reach no dset through this context.
                let text = self.text.as_str_lossy();
                let (inp, out) = if self.field == "INP" {
                    (text.as_ref(), "")
                } else {
                    ("", text.as_ref())
                };
                let ctx = crate::server::ioc_app::DeviceSupportContext {
                    record_type,
                    dtyp: &dtyp,
                    inp,
                    out,
                };
                match db.resolve_device_support(&ctx) {
                    Some(dev) => NewDset::Bound(dev),
                    None => NewDset::Absent,
                }
            };
            self.device = Some(PreparedDevice {
                dtyp,
                dset,
                retired: None,
                dpvt,
                retired_dpvt: crate::server::device_support::Dpvt::Null,
                left_io_intr: false,
                scan_moved: crate::server::record::CommonFieldPutResult::NoChange,
            });
        }

        /// The DTYP moved since [`Self::ask_add_record`] read it, so the
        /// prepared dset is for another device.
        pub(crate) fn device_dtyp_moved(&self, inst: &super::RecordInstance) -> bool {
            self.device
                .as_ref()
                .is_some_and(|d| *d.dtyp != *inst.common.dtyp.as_str())
        }
    }

    impl<T> std::ops::Deref for LinkCache<T> {
        type Target = ParsedLink;

        fn deref(&self) -> &ParsedLink {
            &self.parse
        }
    }

    /// The parse of every link field the record TYPE declares (`OUTA`…,
    /// `LNK1`…, `DOL`, `SVL`, `SELL`), C's `struct link` as `dbInitLink`
    /// leaves it: made when the instance is built from the loaded record and
    /// replaced by a [`PreparedLink`] on each put of the field, as C's
    /// `dbPutFieldLink` re-parses. Never filled by a read, so a reader needs
    /// only `&self` and the record's read lock.
    #[derive(Debug, Default)]
    pub(crate) struct LinkSlots(Vec<LinkSlot>, SimlLset);

    /// Whether SIML's slot holds a link with `dbConst_lset` — the steady
    /// cycle's one SIML question ([`super::RecordInstance::siml_has_const_lset`]),
    /// kept by [`LinkSlots::set`], the only writer of a slot after the build.
    #[derive(Debug)]
    struct SimlLset {
        slot: u8,
        const_lset: bool,
    }

    impl Default for SimlLset {
        /// No SIML: it reads as no link, which is constant.
        fn default() -> Self {
            SimlLset {
                slot: u8::MAX,
                const_lset: true,
            }
        }
    }

    /// Sixteen slotted links by slot index. The slots are in the type's
    /// declaration order, so a row is settled once, when the instance is built.
    #[derive(Debug, Clone, Copy)]
    pub(crate) struct LinkRow([u8; 16]);

    #[derive(Debug)]
    struct LinkSlot {
        /// [`field_key`] of `field`: the scan compares one word per entry.
        key: u64,
        field: &'static str,
        /// [`ParsedLink::Removed`] once C `dbRemoveLink` left the field with
        /// no `lset`; the record keeps the text.
        parsed: Arc<ParsedLink>,
        /// The text parsed, for the debug check that no writer went around
        /// the put funnel.
        #[cfg(debug_assertions)]
        text: PvString,
    }

    /// The first eight bytes of a field name, zero-padded: with the length, a
    /// name of eight bytes or fewer IS its key, and a longer one is also
    /// compared by name on a key match.
    #[inline(always)]
    fn field_key(field: &str) -> u64 {
        let mut b = [0u8; 8];
        let n = field.len().min(8);
        b[..n].copy_from_slice(&field.as_bytes()[..n]);
        u64::from_le_bytes(b)
    }

    /// An unset link — most declared links — shares one allocation, and a
    /// removed one of each type a remove can leave another, made at load
    /// ([`LinkSlots::of`]) so a removal under a record's hold allocates
    /// nothing.
    static SHARED: LazyLock<[Arc<ParsedLink>; 4]> = LazyLock::new(|| {
        [
            Arc::new(ParsedLink::None),
            Arc::new(ParsedLink::Removed(DbLinkType::Constant)),
            Arc::new(ParsedLink::Removed(DbLinkType::PvLink)),
            Arc::new(ParsedLink::Removed(DbLinkType::JsonLink)),
        ]
    });
    const NONE: usize = 0;

    /// The shared [`ParsedLink::Removed`] for `old`: the type `plink->type`
    /// keeps after `dbRemoveLink`. Only a link with an lset is removed
    /// (`dbAccess.c:1150`), and a hardware link has none, so a slot never
    /// holds one here.
    pub(super) fn removed(old: &ParsedLink) -> Arc<ParsedLink> {
        match old.db_link_type() {
            DbLinkType::Constant => SHARED[1].clone(),
            DbLinkType::JsonLink => SHARED[3].clone(),
            ty => {
                debug_assert_eq!(ty, DbLinkType::PvLink, "a removed hardware link");
                SHARED[2].clone()
            }
        }
    }

    fn parse(text: &[u8], ftype: LinkFieldType) -> Arc<ParsedLink> {
        match crate::server::record::parse_link_field(text, ftype) {
            ParsedLink::None => SHARED[NONE].clone(),
            parsed => Arc::new(parsed),
        }
    }

    /// The parser's link type for `d` when it is a link: every link the
    /// record holds has a slot, but a `dbCommon` five the record leaves to
    /// the framework, which has its [`LinkCache`] ([`is_common_link`]).
    fn slot_type(d: &crate::server::record::FieldDesc) -> Option<LinkFieldType> {
        crate::types::link_class_of(d).map(LinkFieldType::for_class)
    }

    fn is_common_link(field: &str) -> bool {
        matches!(field, "INP" | "OUT" | "TSEL" | "SDIS" | "FLNK")
    }

    /// A slotted link's new parse, made before the record's hold is taken —
    /// C's `dbPutFieldLink` parses ahead of `dbScanLock` — so the hold only
    /// moves it in ([`super::RecordInstance::put_link_field`]).
    pub(crate) struct PreparedLink {
        field: &'static str,
        ftype: LinkFieldType,
        parsed: Arc<ParsedLink>,
        /// [`PreparedCommonLink`]'s.
        as_parsed: Arc<ParsedLink>,
        #[cfg(debug_assertions)]
        text: PvString,
    }

    impl PreparedLink {
        /// `Some` when `field` of `rec` is a slotted link and `value` the
        /// string a record stores a link's text from; any other put leaves
        /// the slots, and a non-string one costs no lookup.
        pub(crate) fn for_put(
            fields: &'static [crate::server::record::FieldDesc],
            field: &str,
            value: &crate::types::EpicsValue,
        ) -> Option<Self> {
            let crate::types::EpicsValue::String(text) = value else {
                return None;
            };
            #[cfg(test)]
            super::put_gate_probe::parsed();
            // `fields` is what the record stores, so a `dbCommon` five in it
            // is one the record holds and has a slot ([`LinkSlots::of`]).
            let desc = super::field_desc_in(fields, field)?;
            let ftype = slot_type(desc)?;
            let parsed = parse(text.as_bytes(), ftype);
            Some(PreparedLink {
                field: desc.name,
                ftype,
                as_parsed: Arc::clone(&parsed),
                parsed,
                #[cfg(debug_assertions)]
                text: text.clone(),
            })
        }

        pub(crate) fn field(&self) -> &'static str {
            self.field
        }

        /// The parse before [`Self::relink`]: what C `dbPutFieldLink`
        /// creates its channel from, ahead of `dbAddLink`.
        pub(crate) fn as_parsed(&self) -> &ParsedLink {
            &self.as_parsed
        }

        /// [`PreparedCommonLink::relink`].
        pub(crate) fn relink(
            &mut self,
            relink: impl FnOnce(&ParsedLink, LinkFieldType) -> Option<ParsedLink>,
        ) {
            if let Some(parsed) = relink(&self.parsed, self.ftype) {
                self.parsed = Arc::new(parsed);
            }
        }
    }

    /// [`PreparedLink`]s for a batch of puts stored under one hold.
    pub(crate) struct PreparedLinks(Vec<PreparedLink>);

    impl PreparedLinks {
        /// Each link put of the batch parsed and relinked as a field put is
        /// ([`PvDatabase::add_link`](crate::server::database::PvDatabase)).
        pub(crate) fn for_puts<'a>(
            db: &crate::server::database::PvDatabase,
            rec: &super::RecordCell,
            puts: impl IntoIterator<Item = (&'a str, &'a crate::types::EpicsValue)>,
        ) -> Self {
            PreparedLinks(
                puts.into_iter()
                    .filter_map(|(field, value)| {
                        let mut link = PreparedLink::for_put(rec.rdes().fields, field, value)?;
                        link.relink(|parsed, ftype| db.add_link(parsed, ftype));
                        Some(link)
                    })
                    .collect(),
            )
        }

        /// The first prepared parse of `field`, in batch order.
        pub(crate) fn take(&mut self, field: &str) -> Option<PreparedLink> {
            let i = self
                .0
                .iter()
                .position(|l| l.field.eq_ignore_ascii_case(field))?;
            Some(self.0.remove(i))
        }
    }

    impl LinkSlots {
        /// Every link field `record`'s type declares that has a slot, from
        /// the table [`PreparedLink::for_put`] reads, so a put's parse always
        /// has its slot.
        pub(super) fn of(record: &dyn crate::server::record::Record) -> Self {
            use crate::server::record::FieldDeclaration as _;
            LazyLock::force(&SHARED);
            let fields = record
                .field_list()
                .iter()
                .filter(|d| !is_common_link(d.name) || record.implements_field(d.name))
                .filter_map(|d| Some((d.name, slot_type(d)?)));
            let mut slots = LinkSlots::default();
            for (field, ftype) in fields {
                let text = super::link_text_of(record, field).unwrap_or_default();
                slots.0.push(LinkSlot {
                    key: field_key(field),
                    field,
                    parsed: parse(text.as_bytes(), ftype),
                    #[cfg(debug_assertions)]
                    text,
                });
            }
            if let Some(i) = slots.position("SIML").and_then(|i| u8::try_from(i).ok()) {
                slots.1 = SimlLset {
                    slot: i,
                    const_lset: has_const_lset(&slots.0[usize::from(i)].parsed),
                };
            }
            slots
        }

        /// Slot `i` takes `parsed`: every write after the build comes here.
        fn set(&mut self, i: usize, parsed: Arc<ParsedLink>) {
            let mut parsed = parsed;
            self.swap(i, &mut parsed);
        }

        /// [`Self::set`] handing the old parse back in `parsed`, for a caller
        /// that drops it after the hold.
        pub(super) fn swap(&mut self, i: usize, parsed: &mut Arc<ParsedLink>) {
            if usize::from(self.1.slot) == i {
                self.1.const_lset = has_const_lset(parsed);
            }
            std::mem::swap(&mut self.0[i].parsed, parsed);
        }

        #[inline(always)]
        pub(super) fn siml_has_const_lset(&self) -> bool {
            self.1.const_lset
        }

        /// Parse `text` into slot `i`, for a link whose text lives in the
        /// instance's override store (`mca`'s SIML/SIOL): only the load
        /// writes one, since every such field is `SPC_NOMOD`.
        pub(super) fn install_text(
            &mut self,
            i: usize,
            desc: &crate::server::record::FieldDesc,
            text: &[u8],
        ) {
            let Some(ftype) = slot_type(desc) else {
                return;
            };
            self.set(i, parse(text, ftype));
            #[cfg(debug_assertions)]
            {
                self.0[i].text = text.into();
            }
        }

        /// Move `link` into its slot; the parse was made outside the hold.
        pub(super) fn install(&mut self, link: PreparedLink) {
            let Some(i) = self.position(link.field) else {
                debug_assert!(false, "{}: PreparedLink without a slot", link.field);
                return;
            };
            self.set(i, link.parsed);
            #[cfg(debug_assertions)]
            {
                self.0[i].text = link.text;
            }
        }

        #[inline(always)]
        pub(super) fn position(&self, field: &str) -> Option<usize> {
            let key = field_key(field);
            self.0.iter().position(|e| {
                e.key == key
                    && e.field.len() == field.len()
                    && (field.len() <= 8 || e.field == field)
            })
        }

        pub(super) fn at(&self, i: usize) -> &Arc<ParsedLink> {
            &self.0[i].parsed
        }

        pub(super) fn links(&self) -> impl Iterator<Item = &Arc<ParsedLink>> {
            self.0.iter().map(|slot| &slot.parsed)
        }

        /// C `dbRemoveLink` on slot `i`.
        pub(super) fn remove(&mut self, i: usize) {
            self.set(i, removed(&self.0[i].parsed));
        }

        pub(super) fn is_removed(&self, i: usize) -> bool {
            matches!(*self.0[i].parsed, ParsedLink::Removed(_))
        }

        pub(super) fn field(&self, i: usize) -> &'static str {
            self.0[i].field
        }

        /// `fields`' slots, `None` unless every one has a slot.
        pub(super) fn row(&self, fields: &[&str; 16]) -> Option<LinkRow> {
            let mut row = [0; 16];
            for (slot, field) in row.iter_mut().zip(fields) {
                *slot = u8::try_from(self.position(field)?).ok()?;
            }
            Some(LinkRow(row))
        }

        /// Slot index of link `i` of `row`.
        pub(super) fn row_slot(row: LinkRow, i: usize) -> usize {
            usize::from(row.0[i])
        }

        pub(super) fn field_at(&self, i: usize) -> &'static str {
            self.0[i].field
        }

        /// What a field with no slot reads as: no link.
        pub(super) fn none() -> &'static Arc<ParsedLink> {
            &SHARED[NONE]
        }

        #[cfg(debug_assertions)]
        pub(super) fn text_at(&self, i: usize) -> &[u8] {
            self.0[i].text.as_bytes()
        }

        /// The slot of each [`CycleLink`] the type declares. The slots never
        /// move once built, so the instance settles these at construction.
        pub(super) fn cycle_slots(&self) -> CycleLinkSlots {
            CycleLinkSlots(CycleLink::FIELDS.map(|field| {
                self.position(field)
                    .and_then(|i| u8::try_from(i).ok())
                    .unwrap_or(u8::MAX)
            }))
        }

        /// Slot of `link`, `None` when the type does not declare it.
        #[inline(always)]
        pub(super) fn cycle_slot(slots: CycleLinkSlots, link: CycleLink) -> Option<usize> {
            let i = slots.0[link as usize];
            (i != u8::MAX).then_some(usize::from(i))
        }
    }

    /// A single link field the process cycle reads by a name its type fixes:
    /// the closed-loop and pre-input `DOL`, sel's `NVL`, aSub's `SUBL`, and
    /// the SIM block's `SIML` and `SIOL`.
    #[derive(Debug, Clone, Copy)]
    pub(crate) enum CycleLink {
        Dol,
        Nvl,
        Subl,
        Siml,
        Siol,
    }

    impl CycleLink {
        const FIELDS: [&'static str; 5] = ["DOL", "NVL", "SUBL", "SIML", "SIOL"];
    }

    /// [`LinkSlots`] index of each [`CycleLink`], `u8::MAX` for none.
    #[derive(Debug, Clone, Copy)]
    pub(crate) struct CycleLinkSlots([u8; 5]);

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn slots_tell_apart_names_that_share_an_eight_byte_prefix() {
            let slots = LinkSlots(
                ["ABCDEFGH1", "ABCDEFGH2", "OUT"]
                    .map(|field| LinkSlot {
                        key: field_key(field),
                        field,
                        parsed: parse(b"", LinkFieldType::In),
                        #[cfg(debug_assertions)]
                        text: PvString::new(),
                    })
                    .into(),
                SimlLset::default(),
            );
            assert_eq!(slots.position("ABCDEFGH1"), Some(0));
            assert_eq!(slots.position("ABCDEFGH2"), Some(1));
            assert_eq!(slots.position("OUT"), Some(2));
            assert_eq!(slots.position("OUTA"), None);
            assert_eq!(slots.position("ABCDEFGH"), None);
        }

        /// The SIML gate follows every slot write: a DB link is read, a
        /// removed one too (its read fails `S_db_noLSET`), an empty one not.
        #[test]
        fn the_siml_lset_flag_follows_every_slot_write() {
            use crate::server::record::{FieldDeclaration as _, Record as _};
            let mut ai = crate::server::records::ai::AiRecord::new(0.0);
            ai.put_field("SIML", crate::types::EpicsValue::String("SRC".into()))
                .unwrap();
            let mut slots = LinkSlots::of(&ai);
            assert!(!slots.siml_has_const_lset());
            let i = slots.position("SIML").unwrap();
            slots.remove(i);
            assert!(!slots.siml_has_const_lset());
            let desc = super::super::field_desc_in(ai.field_list(), "SIML").unwrap();
            slots.install_text(i, desc, b"");
            assert!(slots.siml_has_const_lset());
            assert!(LinkSlots::default().siml_has_const_lset(), "no SIML");
        }

        #[test]
        fn cycle_slots_name_the_slot_a_search_finds_or_none() {
            let slots = LinkSlots(
                ["OUT", "DOL", "NVL"]
                    .map(|field| LinkSlot {
                        key: field_key(field),
                        field,
                        parsed: parse(b"", LinkFieldType::In),
                        #[cfg(debug_assertions)]
                        text: PvString::new(),
                    })
                    .into(),
                SimlLset::default(),
            );
            let cycle = slots.cycle_slots();
            assert_eq!(LinkSlots::cycle_slot(cycle, CycleLink::Dol), Some(1));
            assert_eq!(LinkSlots::cycle_slot(cycle, CycleLink::Nvl), Some(2));
            assert_eq!(LinkSlots::cycle_slot(cycle, CycleLink::Subl), None);
        }
    }
}

pub(crate) use link_cache::{
    CycleLink, CycleLinkSlots, LinkRow, LinkSlots, NewDset, OutTarget, PreparedCommonLink,
    PreparedLink, PreparedLinks,
};
pub use link_cache::{LinkCache, OutLinkCache};

/// How one process cycle of one record instance runs, resolved from the
/// instance's own link kinds and device type (where [`ProcessPlan`] holds what
/// the record *type* fixes). Resolved by [`RecordInstance::cycle_plan`] and
/// stale exactly when [`RecordInstance::invalidate_plan`] has run since.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CyclePlan {
    /// Every feature test runs: the cycle as the port always ran it.
    General,
    /// A soft-channel input record whose INP is not read
    /// ([`RecordInstance::dset_reads_inp`]), whose SDIS and SIML are constants and
    /// whose SSCN is unset, with the plan of [`ProcessPlan::PLAIN_INPUT`]: the
    /// cycle with those answers folded in. SIMM and the device slot are not
    /// part of the stamp; [`RecordInstance::cycle_plan`] tests them per cycle.
    ConstSoftIn,
    /// [`Self::ConstSoftIn`] with INP a non-PP DB link read as `DBR_DOUBLE`
    /// by a dset that is not Raw, and TSE not -2: the input stage is the in-guard slot read of
    /// `soft_db_inp_stage` when the target resolves and is not this record,
    /// the general stage otherwise. Any other request is General: the arm's
    /// slot carries an `f64`, so the arm itself is the request.
    DbSoftIn,
}

/// Instance answers a cycle would otherwise test on every process, resolved
/// with the [`CyclePlan`] arm and stale with it: each `true` names a region the
/// cycle skips. [`Default`] skips nothing, which is the General reference.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct CycleFacts {
    /// SIMM is NO, SSCN is unset and SIML and SDIS are constants: C
    /// `recGblGetSimm` reads nothing and `readValue`/`writeValue` take the
    /// real I/O, and no SDIS read has released the guard since the entry
    /// read the put-notify queue.
    pub(crate) not_simulating: bool,
    /// The device posts monitors itself
    /// ([`crate::server::device_support::DeviceSupport::posts_from_device`]):
    /// the cycle posts them after `write_begin_common`.
    pub(crate) device_posts: bool,
}

/// What a record's TYPE can reach in a process cycle.
///
/// C enters a record through one `rset->process` pointer and runs only the
/// lines that type has. This port runs one generic body for every type, so a
/// `calc` cycle otherwise takes a record lock to ask whether device support
/// may refuse it, another for the simulation block, another for `sel`'s NVL
/// and another for the string-input fetch — four locks and four dynamic calls
/// to learn four answers that were fixed when the type was compiled. They are
/// settled once, in [`RecordCell::new`], and live on the cell rather than
/// under its lock: a cycle borrows them ([`RecordCell::process_plan`])
/// instead of copying them out of the guarded instance, which the body did
/// field by field — one load and one spill per flag — on every cycle.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct ProcessPlan {
    /// This type's C `process()` opens with a dset test that can refuse the
    /// cycle (`dev_sup_process_refusal`). `false` for `calc`, `sub`, `fanout`
    /// and `calcout`, whose C `process()` has no such line.
    pub(crate) dset_can_refuse: bool,
    /// This type's dbd declares SIMM — it has a C `readValue`/`writeValue`
    /// simulation block at all — and the four fields that block reaches.
    /// `None` is the type without the block; there is no state in which the
    /// block exists and its fields are unresolved.
    pub(crate) simm: Option<SimmBlock>,
    /// This type is `sel`, the only one with an NVL link feeding SELN.
    pub(crate) sel_nvl: bool,
    /// This type declares at least one string-input link.
    pub(crate) string_input: bool,
    /// This type's `fanout`/`dfanout`/`seq` link array is driven by
    /// `PvDatabase::dispatch_multi_output`, which runs in the pre-commit
    /// output stage (C `dbScanFwdLink`s from `process()` ahead of `monitor()`);
    /// neither forward-link tail reads it.
    pub(crate) multi_output_dispatch: bool,
    /// This type reads `SELL` into `SELN` at some phase of the cycle.
    pub(crate) reads_sell: bool,
    /// This type is `event`, the only one that posts a named software event
    /// from the forward-link tail.
    pub(crate) posts_software_event: bool,
    /// This type is `aSub`, the only one that can re-read its subroutine name
    /// from a link mid-cycle.
    pub(crate) resolves_subroutine_from_link: bool,
    /// This type is `sub` or `aSub`, the only ones with a `do_sub`. `false`
    /// drops [`RecordInstance::run_registered_subroutine`] from the cycle.
    pub(crate) runs_subroutine: bool,
    /// This type's simulation block replaces the input stage rather than
    /// running alongside it — `swait` alone. `false` makes the whole
    /// `set_simulation_active` step disappear, lock included, rather than
    /// taking the record's write lock to ask and be told no.
    pub(crate) substitutes_input_stage_when_simulating: bool,
    /// This type has a `conditional_write` epilogue — see
    /// [`Record::redecides_after_output`]. `false` makes the post-output write
    /// lock disappear.
    pub(crate) redecides_after_output: bool,
    /// [`Record::acts_before_process`]. `false` drops the vtable call to an
    /// empty `pre_process_actions` from every cycle.
    pub(crate) acts_before_process: bool,
    /// [`Record::cycle_hooks`].
    pub(crate) hooks: crate::server::record::CycleHooks,
    /// What a failed multi-input read means to this type — see
    /// [`Record::input_fetch_policy`].
    pub(crate) input_fetch_policy: crate::server::record::InputFetchPolicy,
    /// `printf` alone re-runs `recGblInitConstantLink` on every process, so
    /// its constant inputs deliver every cycle — see
    /// [`Record::constant_inputs_deliver_at_process`].
    pub(crate) constants_deliver_at_process: bool,
    /// Whether a failed multi-input read owes C's `setLinkAlarm` — see
    /// [`Record::multi_input_fetch_is_db_get_link`].
    pub(crate) multi_input_is_db_get_link: bool,
    /// Every declared multi-input link reads [`LinkReadAs::Native`] and none
    /// declares its failure inert — the framework default, which every base
    /// numeric type gives. The fetch loop then carries the answer instead of
    /// asking [`Record::input_link_request`] and
    /// [`Record::input_link_failure_is_inert`] through the vtable per link,
    /// per cycle. Only settled here for a type whose answers are fixed at the
    /// type ([`Record::input_link_answers_fixed_at_type`]).
    pub(crate) multi_inputs_read_native: bool,
    /// This type declares generic multi-output pairs
    /// ([`Record::multi_output_links`]) for the pre-commit output stage to
    /// drive — scalcout, acalcout, aSub, epid. `false` makes
    /// `dispatch_multi_output_values` and the record lock it takes disappear,
    /// rather than acquiring the lock to re-derive the answer from the
    /// record's type name every cycle.
    ///
    /// Disjoint from [`ProcessPlan::multi_output_dispatch`] by construction:
    /// the `fanout`/`dfanout`/`seq` arrays are driven by
    /// `dispatch_multi_output`, and `multi_output_dispatch_owned` excludes
    /// them from the generic block, so no type can answer `true` to both.
    pub(crate) dispatches_generic_multi_output: bool,
    /// This type declares [`Record::multi_input_links`] at all. `false` — every
    /// type but calc, calcout, sel, sub, aSub and the sCalc/aCalc family — makes
    /// the cycle's read of which links are wired a constant instead of a
    /// virtual call and a mask.
    pub(crate) has_multi_input_links: bool,
    /// This type can narrow the cycle's input list — see
    /// [`Record::narrows_input_links`]. `false` makes the guard that asks
    /// disappear.
    pub(crate) narrows_input_links: bool,
    /// This type has an output stage — a device it can write, an `OUT` link,
    /// an `OEVT` event, multi-output links, or a simulation block — so a cycle
    /// composes and drives outputs between `checkAlarms` and the alarm commit.
    /// `false` is that whole stage absent from the type's C `process()`
    /// (`calcRecord.c` has no such lines): the cycle skips it, and with it the
    /// guard boundary the stage's link writes would need.
    pub(crate) output_stage: bool,
    /// Six per-type answers the cycle asked the record for through the
    /// vtable every process; each is a literal in every implementation, so
    /// they are read once here. See the `Record` method of the same name.
    pub(crate) fetches_dol_closed_loop: bool,
    pub(crate) soft_channel_skips_convert: bool,
    pub(crate) skips_timestamp_when_undefined: bool,
    pub(crate) skips_forward_convert_when_undefined: bool,
    /// The type has an `OEVT` field, the only place
    /// [`Record::output_event`] answers from.
    pub(crate) posts_output_event: bool,
    /// See [`Record::declares_output_time_input_links`].
    pub(crate) reads_links_at_output: bool,
    /// A pass whose stamp is withheld holds `TselStamp::Withheld`, which
    /// stores nothing at any point.
    pub(crate) stamp_point: crate::server::record::StampPoint,
    /// The three below are [`Self::stamp_point`] and [`Self::hooks`] folded
    /// into the one test each cycle pays. `stamp_point == BeforeAlarms`.
    pub(crate) stamps_before_alarms: bool,
    /// The cycle's TSEL is not read inline: a withheld stamp
    /// ([`CycleHooks::withholds_cycle_stamp`]) or a late stamp point.
    ///
    /// [`CycleHooks::withholds_cycle_stamp`]: crate::server::record::CycleHooks::withholds_cycle_stamp
    pub(crate) reads_tsel_out_of_line: bool,
    /// Work between the link stage and `monitor()`: the `HAND_POSTED`
    /// drain or the `AfterLinkWrites` stamp.
    pub(crate) works_before_monitor: bool,
    /// An output whose `recGblGetTimeStampSimm(prec, prec->simm, NULL)` runs
    /// before `writeValue` reads SIML, so it sees the previous cycle's SIMM
    /// ([`Record::stamps_after_write`] is the one exception).
    pub(crate) stamps_before_siml_read: bool,
    pub(crate) clears_udf: bool,
    pub(crate) soft_read_clears_udf: bool,
    /// [`Record::can_device_write`], a literal of the type.
    pub(crate) can_device_write: bool,
}

/// Which pass of `process()` a stamp belongs to. The completion pass is the
/// re-entry with `pact` set: `writeValue` returns before `recGblGetSimm`, so
/// its `recGblGetTimeStampSimm` (`aoRecord.c:228`, `lsoRecord.c:154`) sees the
/// SIMM the first pass left.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum StampPass {
    First,
    Completion,
}

impl ProcessPlan {
    /// Whether this pass's output stamp sees the SIMM from before the cycle's
    /// SIML read ([`Self::stamps_before_siml_read`]); never the completion.
    #[inline(always)]
    pub(crate) fn stamp_sees_previous_simm(&self, pass: StampPass) -> bool {
        pass == StampPass::First && self.stamps_before_siml_read
    }
}

/// Link parses made while this thread holds a put gate, for the tests that
/// pin C `dbPutFieldLink`'s parse-before-lock order (`dbAccess.c:1101`,
/// `:1123`). Test builds only.
#[cfg(test)]
pub(crate) mod put_gate_probe {
    use std::cell::Cell;

    thread_local! {
        static DEPTH: Cell<usize> = const { Cell::new(0) };
        static PARSES: Cell<usize> = const { Cell::new(0) };
    }

    /// Lives as long as the put gate that holds it.
    pub(crate) struct Under(());

    impl Under {
        pub(crate) fn enter() -> Self {
            DEPTH.with(|d| d.set(d.get() + 1));
            Under(())
        }
    }

    impl Drop for Under {
        fn drop(&mut self) {
            DEPTH.with(|d| d.set(d.get() - 1));
        }
    }

    /// A link parse: counted when this thread is under a put gate.
    pub(crate) fn parsed() {
        if DEPTH.with(Cell::get) > 0 {
            PARSES.with(|p| p.set(p.get() + 1));
        }
    }

    /// The parses counted under a gate since the last call.
    pub(crate) fn take() -> usize {
        PARSES.with(|p| p.replace(0))
    }
}

/// How many record borrows this thread holds, in builds with debug
/// assertions: the set-wide answer to "may this thread park, or enter a
/// nested frame, now?" that the per-record borrow flag cannot give. A borrow
/// is taken only by the thread holding the record's set, so a thread's count
/// is the count of live borrows in every set it holds. Compiled out of
/// release builds, where the park keeps its per-record check.
pub(crate) mod live_borrows {
    #[cfg(debug_assertions)]
    thread_local! {
        static LIVE: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    }

    #[inline(always)]
    pub(super) fn taken() {
        #[cfg(debug_assertions)]
        LIVE.with(|c| c.set(c.get() + 1));
    }

    #[inline(always)]
    pub(super) fn dropped() {
        #[cfg(debug_assertions)]
        LIVE.with(|c| c.set(c.get() - 1));
    }

    /// Borrows of record data this thread holds now; `0` where not counted.
    #[inline(always)]
    pub(crate) fn count() -> usize {
        #[cfg(debug_assertions)]
        return LIVE.with(|c| c.get());
        #[cfg(not(debug_assertions))]
        0
    }
}

/// The allocation every holder of a record shares — the port's `precord`.
///
/// The record's data is guarded by its **lock set** and nothing else — C's
/// `dbScanLock` (`dbLock.c:184-213`), the one mutex every record a DB link
/// reaches shares. [`Self::read`] and [`Self::write`] take that set
/// (recursively, as C's `epicsMutex` is) and hand out a borrow of the data;
/// a thread already inside the set — the process path, which took it at
/// entry — pays a thread-key compare and a counter, no atomic
/// read-modify-write. The per-record `RwLock` this replaced was a second
/// lock over the same data: every field access on the process path took it
/// again under the set that already excluded every other writer.
///
/// What lives HERE rather than in [`RecordInstance`] is decided by one rule:
/// **a fact a caller needs before it holds the lock cannot live behind the
/// lock.** C has exactly one such fact — `precord->lset`, read off
/// `dbCommon` with no lock at all to learn which mutex to take.
///
/// Same-thread aliasing is what the `RwLock` used to refuse by deadlocking
/// (a `write()` under a live guard of the same record never returned). The
/// set recurses, so that refusal is `borrow`'s now, and it panics —
/// on the thread that did it, naming the record — instead of parking it.
#[repr(C)]
pub struct RecordCell {
    /// First, and `repr(C)` keeps it there: `run_process_frame` reaches the
    /// instance through the cell pointer, and at any other offset every
    /// borrow pays a `lea` (+6 instr/cycle when a `RecordDesc` resize moved
    /// it to 0x40). The cell's own fields a process cycle reads follow it
    /// directly, continuing the instance's hot tail; the rest come last.
    data: LockSetGuarded<RecordInstance>,
    /// C `dbCommon::lset` (`dbLockPvt.h:52`). Born pointing at the bootstrap
    /// set — C's null `lset` before `dbLockInitRecords`, made lockable — and
    /// moved onto a set of its own by the registry, which adopts this very
    /// cell for the record's name so that the record and the registry can
    /// never hold two answers to "which set is this record in". A lock-set
    /// MERGE moves the set the cell points at, never the cell.
    lock_record: std::sync::Arc<crate::server::database::LockRecord>,
    /// `RefCell`-style borrow state of `data`: `0` free, `n > 0` that many
    /// shared borrows, `-1` one exclusive borrow. Touched only by the thread
    /// holding the record's lock set, which is why plain loads and stores
    /// (no read-modify-write) are enough — the atomic is for `Sync`, not for
    /// contention, and there is none.
    borrow: std::sync::atomic::AtomicIsize,
    /// C `cvt_dbaddr` owed by a lock-free `dbNameToAddr`
    /// ([`Self::resolve_addr`]): bit `i` is entry `i` of
    /// [`Self::dbaddr_counts`]. A resolve that finds the record busy cannot
    /// run [`super::Record::cvt_dbaddr`], so it owes the cell to its lock set,
    /// and the set's next fresh hold runs it before anything reads the record.
    cvt_pending: std::sync::atomic::AtomicU64,
    /// The type-static answers a process cycle tests — see [`ProcessPlan`].
    /// Fixed by the type, like `rdes`, so it is reachable without a borrow
    /// of the record.
    plan: ProcessPlan,
    /// C `dbCommon::name`, fixed at construction like `rdes`, so a message
    /// naming the record is built with no borrow of it held.
    name: crate::types::RecordName,
    /// This record's `init_record` is still OWED to `iocInit` — it was created
    /// during the LOAD phase, which defers the whole init half (C runs
    /// `dbLoadRecords` and `initDatabase` as two passes).
    ///
    /// Here for the same reason [`Self::load_order`] is: the db loader asks it
    /// per record, before it takes the record's lock, and what it used to ask
    /// instead was a `Vec<String>` of owed names beside the records —
    /// scanned linearly, so one `.db` cost a scan per record and
    /// `dbLoadRecords` was quadratic a second time over. On the node the
    /// answer is the record's own, and the owed-ness cannot name a record
    /// that does not exist.
    init_owed: std::sync::atomic::AtomicBool,
    /// This record's links still await C `doResolveLinks`' `openLink`
    /// (`iocInit.c:576-577`): claimed once, so each link is opened once at
    /// init however many init walks reach the record.
    link_opens_owed: std::sync::atomic::AtomicBool,
    /// C `processTarget`'s `psrc->pact = TRUE` (`dbDbLink.c:457`, restored at
    /// `:514`): this record is the source of a PP or FLNK target being
    /// processed. Read only as the PACT field (through `RecordCold::cell`),
    /// so a link the target's chain reads back sees 1; it gates no cycle.
    /// Set with no borrow of the record, on the line of `name`, which the
    /// cycle reads.
    chain_pact: AtomicBool,
    /// This record's slot in the database's record table, given at
    /// registration; `None` for a cell no database registered.
    index: Option<crate::server::database::RecordIdx>,
    /// C `dbCommon::rdes` — the record type's description, reachable without
    /// a borrow of the record because a cell's type never changes. What a
    /// link target's field address ([`FieldAddr`]) is resolved against.
    rdes: RecordDesc,
    /// [`Record::field_slot`] for each entry of `rdes.fields`, asked once
    /// here so a link resolving its target's address needs no lock.
    slots: Box<[Option<super::record_trait::FieldSlot>]>,
    /// Beside each `slots` entry, what [`PutAddr::resolve_in`] needs of the
    /// record for a put, asked once here with the instance owned.
    put_facts: Box<[PutFacts]>,
    /// [`Record::field_slot`] of each [`Record::multi_input_links`] value
    /// field, by the link's index — C's `&prec->a + i`, the address the
    /// fetch stores through ([`Record::put_slot_f64`]) when the type hands
    /// one out.
    multi_input_val_slots: Box<[Option<super::record_trait::FieldSlot>]>,
    /// C `dbRecordNode::order` (`dbStaticLib.c:1704`) — this record's place in
    /// the database load sequence: the scan index's secondary sort key, and
    /// the order every whole-database walk visits.
    ///
    /// Here by the rule above: every reader of it — the scan-index insert,
    /// `all_record_names`, the PINI sweep — needs it *before* it takes the
    /// record's lock set, and what it used to reach for instead was a
    /// name-keyed copy-on-write map beside the records. That map was cloned
    /// once per record added, which made `dbLoadRecords` quadratic. On the
    /// cell the sequence is born with the node, cannot go missing from it, and
    /// dies with it.
    load_order: u64,
    /// The record's `BKPT` byte, shared with `data.common.bkpt` — reachable
    /// here without the lock set, as C reads `precord->bkpt` (see
    /// [`BkptFlag`]).
    bkpt: BkptFlag,
    /// Bit `i`: the `i`th [`FieldDesc::runtime_typed`] field of `rdes.fields`
    /// is served as `DBF_STRING` (`cvt_dbaddr` from `FTVL`, `FTA`...). Those
    /// selectors are `SPC_NOMOD`, so the fact is settled where they can still
    /// move ([`Self::settle_addr_facts`]) and read with no lock.
    string_typed: std::sync::atomic::AtomicU64,
    /// C `paddr->no_elements` of each `SPC_DBADDR` field, by its index in
    /// `rdes.fields`: what `cvt_dbaddr` answers, settled with
    /// [`Self::string_typed`] so `dbNameToAddr` reads it with no lock, as C
    /// reads `prec->nelm`. The capacity fields are `SPC_NOMOD`.
    dbaddr_counts: arc_swap::ArcSwap<Vec<(u16, u32)>>,
    /// The instance's [`RecordCold::metadata_cache`], shared: what the record
    /// reads of its own metadata from inside its processing
    /// ([`own_field_snapshot`]), with no lock.
    metadata: MetadataCell,
}

const _: () = assert!(std::mem::offset_of!(RecordCell, data) == 0);

/// [`RecordCell::dbaddr_counts`] of `inst`.
fn dbaddr_counts_of(inst: &RecordInstance) -> Vec<(u16, u32)> {
    let rec = &*inst.record;
    rec.field_list()
        .iter()
        .enumerate()
        .filter(|(_, f)| !f.long_string && rec.field_is_dbaddr(f.name))
        .map(|(i, f)| {
            let count = rec
                .dbaddr_count(f.name)
                .or_else(|| inst.resolve_field(f.name).map(|v| v.count() as u32))
                .unwrap_or(1);
            (i as u16, count)
        })
        .collect()
}

/// [`RecordCell::string_typed`] of `inst`.
fn string_typed_mask(inst: &RecordInstance) -> u64 {
    use crate::types::DbfCode;
    let runtime_typed = inst.record.field_list().iter().filter(|f| f.runtime_typed);
    runtime_typed.enumerate().fold(0, |mask, (i, f)| {
        debug_assert!(i < 64, "{} has over 64 runtime-typed fields", f.name);
        let value = inst.resolve_field(f.name);
        match crate::server::database::char_view::field_addr_types(f, value.as_ref()).0 {
            DbfCode::String => mask | 1 << i,
            _ => mask,
        }
    })
}

/// A record type's declaration, C's `dbRecordType` behind `precord->rdes`:
/// the two static tables a field name is resolved against.
#[derive(Clone, Copy)]
pub(crate) struct RecordDesc {
    pub(crate) record_type: &'static str,
    pub(crate) fields: &'static [FieldDesc],
    /// [`FieldDeclaration::noaccess_names`].
    noaccess: &'static [&'static str],
    /// [`Record::primary_field`] and [`Record::process_passive_fields`].
    primary_field: &'static str,
    pp_fields: &'static [&'static str],
    /// [`Record::rederives_udf_on_computed_read`]. Kept here, not in
    /// [`ProcessPlan`], because `ai` and `bi` answer it differently and
    /// share [`ProcessPlan::PLAIN_INPUT`].
    rederives_udf_on_computed_read: bool,
    /// [`Record::refused_async_read_keeps_udf`], here for the same reason.
    refused_async_read_keeps_udf: bool,
    /// [`Record::raw_soft_read_failure_skips_convert`], here for the same reason.
    raw_soft_read_failure_skips_convert: bool,
    /// [`Record::process_status_rule`], here for the same reason.
    process_status: crate::server::record::ProcessStatusRule,
    /// [`Record::string_input_links`].
    string_input_links: &'static [(&'static str, &'static str)],
}

impl RecordDesc {
    /// Whether `field` is in [`Record::process_passive_fields`].
    #[inline]
    pub(crate) fn is_pp(&self, field: &str) -> bool {
        self.pp_fields.iter().any(|f| f.eq_ignore_ascii_case(field))
    }

    /// Whether `field` is the type's [`Record::primary_field`]. `record` is
    /// the cell's own record, asked only by a debug build to check the fact.
    #[inline]
    pub(crate) fn is_primary(&self, field: &str, record: &dyn Record) -> bool {
        debug_assert_eq!(self.primary_field, record.primary_field());
        field == self.primary_field
    }

    /// [`Record::rederives_udf_on_computed_read`]; `record` as in
    /// [`Self::is_primary`].
    #[inline]
    pub(crate) fn rederives_udf_on_computed_read(&self, record: &dyn Record) -> bool {
        debug_assert_eq!(
            self.rederives_udf_on_computed_read,
            record.rederives_udf_on_computed_read()
        );
        self.rederives_udf_on_computed_read
    }

    /// [`Record::refused_async_read_keeps_udf`]; `record` as in [`Self::is_primary`].
    #[inline]
    pub(crate) fn refused_async_read_keeps_udf(&self, record: &dyn Record) -> bool {
        debug_assert_eq!(
            self.refused_async_read_keeps_udf,
            record.refused_async_read_keeps_udf()
        );
        self.refused_async_read_keeps_udf
    }

    /// [`Record::raw_soft_read_failure_skips_convert`]; `record` as in [`Self::is_primary`].
    #[inline]
    pub(crate) fn raw_soft_read_failure_skips_convert(&self, record: &dyn Record) -> bool {
        debug_assert_eq!(
            self.raw_soft_read_failure_skips_convert,
            record.raw_soft_read_failure_skips_convert()
        );
        self.raw_soft_read_failure_skips_convert
    }

    /// [`Record::process_status_rule`]; `record` as in [`Self::is_primary`].
    #[inline]
    pub(crate) fn process_status(
        &self,
        record: &dyn Record,
    ) -> crate::server::record::ProcessStatusRule {
        debug_assert_eq!(self.process_status, record.process_status_rule());
        self.process_status
    }

    /// [`Record::string_input_links`]; `record` as in [`Self::is_primary`].
    #[inline]
    pub(crate) fn string_input_links(
        &self,
        record: &dyn Record,
    ) -> &'static [(&'static str, &'static str)] {
        debug_assert_eq!(self.string_input_links, record.string_input_links());
        self.string_input_links
    }

    pub(crate) fn of<R: Record + ?Sized>(record: &R) -> Self {
        RecordDesc {
            record_type: record.record_type(),
            fields: record.field_list(),
            noaccess: record.noaccess_names(),
            primary_field: record.primary_field(),
            pp_fields: record.process_passive_fields(),
            rederives_udf_on_computed_read: record.rederives_udf_on_computed_read(),
            refused_async_read_keeps_udf: record.refused_async_read_keeps_udf(),
            raw_soft_read_failure_skips_convert: record.raw_soft_read_failure_skips_convert(),
            process_status: record.process_status_rule(),
            string_input_links: record.string_input_links(),
        }
    }
}

/// C's `dbAddr` field half — `pfldDes`, plus whether the name is one the
/// type DECLARES (an undeclared one falls to the attribute table, C
/// `dbNameToAddr`'s `dbGetAttributePart` fallthrough, `dbAccess.c:667-675`).
/// Settled once per link target and read through every cycle after, as C
/// resolves a link's `dbAddr` once in `dbDbInitLink`.
#[derive(Clone, Copy)]
pub(crate) struct FieldAddr {
    pub(crate) declared: bool,
    pub(crate) desc: Option<&'static FieldDesc>,
    slot: Option<super::record_trait::FieldSlot>,
    char_view: bool,
}

impl FieldAddr {
    /// `field` (upper-case already) resolved against the cell's settled
    /// declaration and slots. Takes no lock, so a record may resolve a link
    /// to itself while it is held for writing.
    pub(crate) fn resolve_in(rec: &RecordCell, field: &str) -> Self {
        Self::resolve_at(rec, field).0
    }

    /// [`Self::resolve_in`] and the field's index in `rdes.fields`.
    fn resolve_at(rec: &RecordCell, field: &str) -> (Self, Option<usize>) {
        // One scan of the type's own list answers both the desc and, by the
        // same index, the slot; only a miss falls to `dbCommon`.
        let named = |f: &FieldDesc| f.name.eq_ignore_ascii_case(field);
        let fields = rec.rdes().fields;
        let declared = declares_field(rec.rdes().record_type, field);
        // `named` ignores case and `declares_field` does not (C
        // `dbFindFieldPart`'s `strncmp`), so a mis-cased name has no slot.
        let at = fields.iter().position(named);
        let addr = FieldAddr {
            declared,
            desc: at.map(|i| &fields[i]).or_else(|| {
                super::dbd_generated::DB_COMMON_FIELDS
                    .iter()
                    .find(|f| named(f))
            }),
            slot: at.filter(|_| declared).and_then(|i| rec.slots[i]),
            char_view: false,
        };
        debug_assert!(
            addr.slot.is_none() || (addr.declared && addr.desc.is_some_and(|d| !d.unreadable())),
            "{}.{field}: a field slot must name a declared, readable field",
            rec.rdes().record_type
        );
        (addr, at.filter(|_| declared))
    }

    /// The record's own handle for the field ([`Record::field_slot`]), when
    /// the type has one and the name carried no `$`. A read at a slot goes
    /// to the typed accessor and skips the by-name chain.
    #[inline(always)]
    pub(crate) fn slot(&self) -> Option<super::record_trait::FieldSlot> {
        self.slot
    }

    /// The name carried C's `$` modifier: `dbNameToAddr` re-made the address
    /// a `DBF_CHAR` array (`dbAccess.c:682-701`), which every read takes
    /// through [`char_view`](crate::server::database::char_view::char_view).
    #[inline(always)]
    pub(crate) fn char_view(&self) -> bool {
        self.char_view
    }
}

/// How `dbPut` stores into a [`PutAddr`]: C's `dbFastPutConvertRoutine`
/// column for the field (`dbFastLinkConv.c:1698`), chosen once with the
/// address. A lean arm stores through [`Record::slot_mut`] with no name
/// work; `General` is the full `dbPut` body.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PutFn {
    General,
    F64(super::record_trait::FieldSlot),
    F32(super::record_trait::FieldSlot),
    I8(super::record_trait::FieldSlot),
    U8(super::record_trait::FieldSlot),
    I16(super::record_trait::FieldSlot),
    U16(super::record_trait::FieldSlot),
    I32(super::record_trait::FieldSlot),
    U32(super::record_trait::FieldSlot),
    I64(super::record_trait::FieldSlot),
    U64(super::record_trait::FieldSlot),
    /// `DBF_ENUM`, a `U16` slot.
    Enum(super::record_trait::FieldSlot),
    /// `DBF_STRING`; `cap` is `putStringString`'s `field_size - 1`.
    Str {
        slot: super::record_trait::FieldSlot,
        cap: u16,
    },
}

/// A [`SlotMut`](super::record_trait::SlotMut) variant, recorded per slot.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SlotKind {
    F64,
    F32,
    I8,
    U8,
    I16,
    U16,
    I32,
    U32,
    I64,
    U64,
    Str,
}

impl SlotKind {
    pub(crate) fn of(slot: &super::record_trait::SlotMut<'_>) -> Self {
        use super::record_trait::SlotMut as M;
        match slot {
            M::F64(_) => Self::F64,
            M::F32(_) => Self::F32,
            M::I8(_) => Self::I8,
            M::U8(_) => Self::U8,
            M::I16(_) => Self::I16,
            M::U16(_) => Self::U16,
            M::I32(_) => Self::I32,
            M::U32(_) => Self::U32,
            M::I64(_) => Self::I64,
            M::U64(_) => Self::U64,
            M::Str(_) => Self::Str,
        }
    }
}

/// What a put needs of the record at a declared field, asked by
/// [`RecordCell::new`] while it owns the instance.
#[derive(Clone, Copy)]
struct PutFacts {
    /// The slot's [`SlotMut`](super::record_trait::SlotMut) variant.
    kind: Option<SlotKind>,
    /// [`Record::is_udf_defining_put`], C `dbIsValueField`.
    udf: bool,
}

/// `putStringString`'s copy length for a `DBF_STRING` field: `field_size -
/// 1`, never over `MAX_STRING_SIZE - 1`.
fn str_cap(desc: &FieldDesc) -> u16 {
    desc.size.clamp(1, 40) - 1
}

/// Each declared field's [`PutFacts`], with every `Str` slot reserved to its
/// cap here, so a lean store never grows it under a hold.
fn put_facts(
    inst: &mut RecordInstance,
    slots: &[Option<super::record_trait::FieldSlot>],
) -> Box<[PutFacts]> {
    use super::record_trait::SlotMut;
    let fields = inst.record.field_list();
    fields
        .iter()
        .zip(slots)
        .map(|(desc, slot)| PutFacts {
            udf: inst.record.is_udf_defining_put(desc.name),
            kind: slot
                .and_then(|slot| inst.record.slot_mut(slot))
                .map(|m| match m {
                    SlotMut::Str(s) => {
                        let cap = usize::from(str_cap(desc));
                        s.reserve_exact(cap.saturating_sub(s.len()));
                        SlotKind::Str
                    }
                    m => SlotKind::of(&m),
                }),
        })
        .collect()
}

/// The lean arm for `desc` at `slot`, or `General` (spec §1.3): anything
/// with a put hook, a `dbCommon` or menu/device/link field, a runtime type,
/// a watched write, or a slot whose recorded variant is not the declared
/// type's.
fn lean_put_fn(
    record_type: &str,
    desc: &'static FieldDesc,
    slot: super::record_trait::FieldSlot,
    kind: SlotKind,
) -> PutFn {
    use super::record_trait::Special;
    use crate::types::DbfCode as D;
    let general = desc.special != Special::None
        || desc.declared_special != Special::None
        || desc.runtime_typed
        || desc.long_string
        || desc.read_only
        || desc.dbf_type.dbf_code() != desc.declared_dbf
        || is_metadata_cache_source(desc.name)
        || RecordInstance::field_posts_property(Some(desc))
        || super::record_trait::FORCE_GENERAL.contains(&(record_type, desc.name));
    if general {
        return PutFn::General;
    }
    match (desc.declared_dbf, kind) {
        (D::Double, SlotKind::F64) => PutFn::F64(slot),
        (D::Float, SlotKind::F32) => PutFn::F32(slot),
        (D::Char, SlotKind::I8) => PutFn::I8(slot),
        (D::UChar, SlotKind::U8) => PutFn::U8(slot),
        (D::Short, SlotKind::I16) => PutFn::I16(slot),
        (D::UShort, SlotKind::U16) => PutFn::U16(slot),
        (D::Enum, SlotKind::U16) => PutFn::Enum(slot),
        (D::Long, SlotKind::I32) => PutFn::I32(slot),
        (D::ULong, SlotKind::U32) => PutFn::U32(slot),
        (D::Int64, SlotKind::I64) => PutFn::I64(slot),
        (D::UInt64, SlotKind::U64) => PutFn::U64(slot),
        // The general arm caps at `size - 1` and leaves an unsized field
        // uncapped; `str_cap` agrees only for a size in `1..=40`.
        (D::String, SlotKind::Str) if (1..=40).contains(&desc.size) => PutFn::Str {
            slot,
            cap: str_cap(desc),
        },
        _ => PutFn::General,
    }
}

/// A `dbPut` target: the [`FieldAddr`] plus the per-field answers `dbPut`
/// asks on every put, settled with the address so a put asks none by name.
/// Built only by [`Self::resolve_in`] and [`Self::resolve_chars_in`], so the
/// store arm cannot be re-made after the address is.
#[derive(Clone, Copy)]
pub(crate) struct PutAddr {
    field: FieldAddr,
    put: PutFn,
    /// C `dbIsValueField`: the put clears `udf`.
    udf_defining: bool,
    /// The declaration's half of [`RecordInstance::is_no_mod`].
    no_mod: bool,
    /// [`is_metadata_cache_source`].
    cache_source: bool,
    /// [`super::FieldDeclaration::field_is_dbaddr`]: `dbPut` takes its
    /// array arm whatever the request's count.
    dbaddr: bool,
    /// A write the faac1df1 change test watches: `cache_source`, or a
    /// `prop(YES)` field ([`RecordInstance::field_posts_property`]).
    watched: bool,
    /// The value field when it is `pp(TRUE)`, whose put-time post the
    /// processing cycle makes instead (`dbput_post_put_field`).
    post_suppressed: bool,
}

/// What [`RecordInstance::notify_field_written_if_changed_at`] compares a put
/// against: the field's value before it, captured only for a watched field
/// ([`RecordInstance::pre_put_at`]).
pub(crate) struct PrePut(Option<Option<EpicsValue>>);

impl PutAddr {
    /// `field` is upper-case already. Takes no lock, as
    /// [`FieldAddr::resolve_in`].
    pub(crate) fn resolve_in(rec: &RecordCell, field: &str) -> Self {
        let (addr, at) = FieldAddr::resolve_at(rec, field);
        let facts = at.map(|i| rec.put_facts[i]);
        let put = match (addr.desc, addr.slot, facts.and_then(|f| f.kind)) {
            (Some(desc), Some(slot), Some(kind)) => {
                lean_put_fn(rec.rdes().record_type, desc, slot, kind)
            }
            _ => PutFn::General,
        };
        // A type with no field list still has `Record::is_udf_defining_put`'s
        // default answer, its primary field.
        let udf = facts.map_or(field == rec.rdes().primary_field, |f| f.udf);
        Self::of(rec, field, addr, put, udf)
    }

    /// A by-name put's address: [`RecordCell::put_name`]'s field, through
    /// [`Self::resolve_chars_in`] when the name carries `$`.
    pub(crate) fn resolve_named<'f>(
        rec: &RecordCell,
        field: &'f str,
    ) -> crate::error::CaResult<(Cow<'f, str>, Self)> {
        let (field, chars) = rec.put_name(field);
        let at = if chars {
            Self::resolve_chars_in(rec, &field)
                .ok_or_else(|| crate::error::CaError::FieldNotFound(format!("{field}$")))?
        } else {
            Self::resolve_in(rec, &field)
        };
        Ok((field, at))
    }

    /// C `dbNameToAddr`'s `$` arm (`dbAccess.c:682-701`): the field re-made
    /// as characters, with no slot and the general put, or `None`
    /// (`S_dbLib_fieldNotFound`, `:697-699`) for a field
    /// [`RecordCell::takes_string_view`] refuses.
    pub(crate) fn resolve_chars_in(rec: &RecordCell, field: &str) -> Option<Self> {
        let (addr, at) = FieldAddr::resolve_at(rec, field);
        if addr.desc.is_some_and(|d| !rec.takes_string_view(d)) {
            return None;
        }
        let udf = at.map_or(field == rec.rdes().primary_field, |i| rec.put_facts[i].udf);
        let addr = FieldAddr {
            slot: None,
            char_view: true,
            ..addr
        };
        Some(Self::of(rec, field, addr, PutFn::General, udf))
    }

    fn of(rec: &RecordCell, field: &str, addr: FieldAddr, put: PutFn, udf: bool) -> Self {
        let rdes = rec.rdes();
        PutAddr {
            put,
            udf_defining: udf,
            dbaddr: addr
                .desc
                .is_some_and(|d| d.declared_special == super::record_trait::Special::DbAddr),
            no_mod: declared_no_mod(field, addr.desc),
            watched: is_metadata_cache_source(field)
                || RecordInstance::field_posts_property(addr.desc),
            cache_source: is_metadata_cache_source(field),
            post_suppressed: field == rdes.primary_field && rdes.is_pp(field),
            field: addr,
        }
    }

    /// The field half of the address.
    #[inline(always)]
    pub(crate) fn field(&self) -> FieldAddr {
        self.field
    }

    /// The store arm `dbPut` takes.
    #[inline(always)]
    pub(crate) fn put(&self) -> PutFn {
        self.put
    }

    /// Whether the put clears `udf` (C `dbIsValueField`).
    #[inline(always)]
    pub(crate) fn udf_defining(&self) -> bool {
        self.udf_defining
    }

    /// Whether `dbPut` takes its array arm for this field whatever the
    /// request's count.
    pub(crate) fn dbaddr(&self) -> bool {
        self.dbaddr
    }

    /// Whether `dbPut` leaves this field's put-time post to the cycle.
    pub(crate) fn post_suppressed(&self) -> bool {
        self.post_suppressed
    }
}

/// The one owner of the published log and of every entry's walk answers:
/// [`PostedLog`] files entries by [`PostKey`], and [`PostedSet`] alone writes
/// an entry's `posted` bit and its list [`Roles`]. Readers settle first
/// through [`RecordInstance::settle_posted`] and
/// [`RecordInstance::settle_roles`]; a proof value carried into the walk's
/// per-entry read cost ao's monitor_cycle 5 instructions per cycle.
mod posted {
    use super::*;
    use crate::server::record::{DeclaredFields, PostTable, ValuePostGate};

    /// What a published value is filed under: a declared field's [`FieldIx`],
    /// or a spelling no declaration matches exactly. A declared field posted
    /// under its own name is always `Ix`, so the log, the skip list and the
    /// walk never read its name.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub(super) enum PostKey<'a> {
        Ix(FieldIx),
        Named(&'a str),
    }

    const FORCED: u8 = 1;
    const FANOUT: u8 = 2;
    const SWEPT: u8 = 4;

    /// One field's answers to the walk's per-field questions: the plan's,
    /// settled when the entry is made, and the record's lists', settled when
    /// one turns over. The walk tests bits.
    #[derive(Debug, Clone, Copy, Default)]
    pub(super) struct Roles {
        /// The field's [`Record::fields_posted_with_value_mask`] gate.
        pub(super) gate: Option<ValuePostGate>,
        /// The field's [`AuxPostMask`] class.
        aux: u8,
        lists: u8,
        /// The entries of the settled mark table that name the field.
        pub(super) marks: u128,
    }

    impl Roles {
        /// A field's roles, `fixed` from the type's lists and the rest by its
        /// declaration `member` under `lists`. Runs only when an entry is made
        /// or a list turns over.
        pub(super) fn of(member: Option<FieldIx>, fixed: FixedRoles, lists: &Lists) -> Self {
            let mut roles = Self {
                gate: fixed.gate,
                aux: fixed.aux,
                ..Self::default()
            };
            roles.settle(member, lists);
            roles
        }

        fn settle(&mut self, member: Option<FieldIx>, lists: &Lists) {
            let has = |list: DeclaredFields, bit| if holds(list, member) { bit } else { 0 };
            self.lists =
                has(lists.forced, FORCED) | has(lists.fanout, FANOUT) | has(lists.swept, SWEPT);
            self.marks = member.map_or(0, |member| {
                lists
                    .marks
                    .iter()
                    .take(u128::BITS as usize)
                    .enumerate()
                    .filter(|(_, (mark, _))| *mark == member)
                    .fold(0, |bits, (i, _)| bits | 1 << i)
            });
        }

        /// [`Record::force_posted_fields`] holds the field.
        pub(super) fn forced(&self) -> bool {
            self.lists & FORCED != 0
        }

        /// [`Record::alarm_cycle_monitored_fields`] holds the field.
        pub(super) fn fanout(&self) -> bool {
            self.lists & FANOUT != 0
        }

        /// [`Record::log_swept_fields`] holds the field.
        pub(super) fn swept(&self) -> bool {
            self.lists & SWEPT != 0
        }

        /// [`AuxPostMask::mask_of_class`] of the field's class.
        pub(super) fn aux_mask(
            &self,
            alarm_bits: EventMask,
            deadband_mask: EventMask,
        ) -> EventMask {
            AuxPostMask::mask_of_class(self.aux, alarm_bits, deadband_mask)
        }
    }

    /// The record's lists every entry's [`Roles`] were settled against, by
    /// identity: a static list is settled once.
    #[derive(Debug, Clone, Copy)]
    pub(super) struct Lists {
        pub(super) forced: DeclaredFields,
        pub(super) fanout: DeclaredFields,
        pub(super) swept: DeclaredFields,
        pub(super) marks: PostTable,
    }

    impl Lists {
        const NONE: Self = Self {
            forced: &[],
            fanout: &[],
            swept: &[],
            marks: &[],
        };

        #[inline(always)]
        fn same(&self, other: &Self) -> bool {
            std::ptr::eq(self.forced, other.forced)
                && std::ptr::eq(self.fanout, other.fanout)
                && std::ptr::eq(self.swept, other.swept)
                && std::ptr::eq(self.marks, other.marks)
        }
    }

    /// A published-log entry: the value last published to the field, and the
    /// field's declaration, settled when the field was first published so the
    /// monitor walk reads it without a name scan. `upper`: the field's name
    /// is already upper-case, as [`RecordInstance::resolve_field_upper_at`]
    /// needs. `prev_bit`: the field's bit in [`Record::prev_compared_inputs`],
    /// settled with the entry so the walk never searches that list.
    #[derive(Debug)]
    pub(super) struct Published {
        pub(super) value: PostedValue,
        pub(super) desc: Option<&'static FieldDesc>,
        pub(super) upper: bool,
        pub(super) prev_bit: Option<u8>,
        roles: Roles,
        posted: bool,
    }

    impl Published {
        /// The field's membership in the list of the last settle.
        #[inline(always)]
        pub(super) fn posted(&self) -> bool {
            self.posted
        }

        #[inline(always)]
        pub(super) fn roles(&self) -> &Roles {
            &self.roles
        }
    }

    /// The published values the walk change-detects against: declared fields
    /// in [`FieldIx`] order, C's `papFldDes` order, which is the order the
    /// walk visits — and posts — them in.
    #[derive(Debug, Default)]
    pub(super) struct PostedLog {
        declared: Vec<(FieldIx, Published)>,
        named: Vec<(Box<str>, Published)>,
    }

    impl PostedLog {
        pub(super) fn is_empty(&self) -> bool {
            self.declared.is_empty() && self.named.is_empty()
        }

        pub(super) fn get(&self, key: PostKey<'_>) -> Option<&Published> {
            match key {
                PostKey::Ix(ix) => self
                    .declared
                    .binary_search_by_key(&ix.0, |(at, _)| at.0)
                    .ok()
                    .map(|i| &self.declared[i].1),
                PostKey::Named(name) => self
                    .named
                    .iter()
                    .find(|(at, _)| name_bytes_eq(at, name))
                    .map(|(_, p)| p),
            }
        }

        pub(super) fn get_mut(&mut self, key: PostKey<'_>) -> Option<&mut Published> {
            match key {
                PostKey::Ix(ix) => self
                    .declared
                    .binary_search_by_key(&ix.0, |(at, _)| at.0)
                    .ok()
                    .map(|i| &mut self.declared[i].1),
                PostKey::Named(name) => self
                    .named
                    .iter_mut()
                    .find(|(at, _)| name_bytes_eq(at, name))
                    .map(|(_, p)| p),
            }
        }

        pub(super) fn contains(&self, key: PostKey<'_>) -> bool {
            self.get(key).is_some()
        }

        pub(super) fn insert(&mut self, key: PostKey<'_>, published: Published) {
            match key {
                PostKey::Ix(ix) => {
                    let at = self.declared.partition_point(|(i, _)| i.0 < ix.0);
                    self.declared.insert(at, (ix, published));
                }
                PostKey::Named(name) => self.named.push((name.into(), published)),
            }
        }

        /// Every entry with its field's name, its key and, for a declared
        /// field, its declaration; declared fields first, in [`FieldIx`] order.
        pub(super) fn entries(
            &self,
        ) -> impl Iterator<Item = (&str, PostKey<'_>, Option<&'static FieldDesc>, &Published)>
        {
            self.declared
                .iter()
                .map(|(ix, p)| (p.desc.map_or("", |d| d.name), PostKey::Ix(*ix), p.desc, p))
                .chain(
                    self.named
                        .iter()
                        .map(|(name, p)| (&**name, PostKey::Named(name), None, p)),
                )
        }

        /// Every entry with the index a declared-field set holds it by.
        fn iter_mut(&mut self) -> impl Iterator<Item = (Option<FieldIx>, &mut Published)> {
            self.declared
                .iter_mut()
                .map(|(ix, p)| (Some(*ix), p))
                .chain(self.named.iter_mut().map(|(_, p)| (None, p)))
        }
    }

    /// Published values of the fields the walk always skips
    /// ([`walk_skip_term`]), filed as [`PostedLog`] files its own.
    #[derive(Debug, Default)]
    pub(super) struct SkippedLog {
        declared: Vec<(FieldIx, PostedValue)>,
        named: Vec<(Box<str>, PostedValue)>,
    }

    impl SkippedLog {
        #[cfg(test)]
        pub(super) fn is_empty(&self) -> bool {
            self.len() == 0
        }

        #[cfg(test)]
        pub(super) fn len(&self) -> usize {
            self.declared.len() + self.named.len()
        }

        pub(super) fn get(&self, key: PostKey<'_>) -> Option<&PostedValue> {
            match key {
                PostKey::Ix(ix) => self
                    .declared
                    .iter()
                    .find(|(at, _)| *at == ix)
                    .map(|(_, v)| v),
                PostKey::Named(name) => self
                    .named
                    .iter()
                    .find(|(at, _)| name_bytes_eq(at, name))
                    .map(|(_, v)| v),
            }
        }

        pub(super) fn set(&mut self, key: PostKey<'_>, value: PostedValue) {
            match key {
                PostKey::Ix(ix) => match self.declared.iter_mut().find(|(at, _)| *at == ix) {
                    Some(slot) => slot.1 = value,
                    None => self.declared.push((ix, value)),
                },
                PostKey::Named(name) => {
                    match self
                        .named
                        .iter_mut()
                        .find(|(at, _)| name_bytes_eq(at, name))
                    {
                        Some(slot) => slot.1 = value,
                        None => self.named.push((name.into(), value)),
                    }
                }
            }
        }
    }

    /// The [`Record::process_posted_fields`] answer every entry's `posted` was
    /// settled against, and the [`Lists`] every entry's roles were settled
    /// against.
    #[derive(Debug)]
    pub(super) struct PostedSet {
        list: Option<DeclaredFields>,
        lists: Lists,
    }

    impl PostedSet {
        pub(super) const fn new() -> Self {
            Self {
                list: None,
                lists: Lists::NONE,
            }
        }

        /// A new entry, its membership and roles taken from the lists every
        /// other entry was settled against, so the next settle moves it with
        /// them.
        pub(super) fn publish(
            &self,
            field: &str,
            value: PostedValue,
            desc: Option<&'static FieldDesc>,
            member: Option<FieldIx>,
            fixed: FixedRoles,
        ) -> Published {
            Published {
                value,
                desc,
                upper: !field.bytes().any(|b| b.is_ascii_lowercase()),
                prev_bit: fixed.prev_bit,
                roles: Roles::of(member, fixed, &self.lists),
                posted: self.contains(member),
            }
        }

        /// A monitor's roles, settled as an entry's.
        pub(super) fn roles_of(&self, member: Option<FieldIx>, fixed: FixedRoles) -> Roles {
            Roles::of(member, fixed, &self.lists)
        }

        /// Whether every entry is already settled against `list`, the
        /// record's answer now.
        #[inline(always)]
        pub(super) fn is_settled(&self, list: Option<DeclaredFields>) -> bool {
            match (list, self.list) {
                (Some(list), Some(posted)) => std::ptr::eq(list, posted),
                (list, posted) => list.is_none() && posted.is_none(),
            }
        }

        /// Settle `log` against `list`.
        pub(super) fn resettle(&mut self, list: Option<DeclaredFields>, log: &mut PostedLog) {
            self.list = list;
            for (member, published) in log.iter_mut() {
                published.posted = self.contains(member);
            }
        }

        /// Whether every entry's roles are settled against `lists`.
        #[inline(always)]
        pub(super) fn lists_settled(&self, lists: &Lists) -> bool {
            self.lists.same(lists)
        }

        /// The lists the roles were last settled against.
        #[inline(always)]
        pub(super) fn lists(&self) -> Lists {
            self.lists
        }

        /// Settle the roles of `log` and of `monitors` against `lists`.
        pub(super) fn resettle_lists<'m>(
            &mut self,
            lists: Lists,
            log: &mut PostedLog,
            monitors: impl Iterator<Item = (Option<FieldIx>, &'m mut Roles)>,
        ) {
            self.lists = lists;
            for (member, published) in log.iter_mut() {
                published.roles.settle(member, &lists);
            }
            for (member, roles) in monitors {
                roles.settle(member, &lists);
            }
        }

        /// The membership of the field declared `member` in the list of the
        /// last settle.
        pub(super) fn contains(&self, member: Option<FieldIx>) -> bool {
            self.list.is_some_and(|list| holds(list, member))
        }
    }
}
use posted::{Lists, PostKey, PostedLog, PostedSet, Roles, SkippedLog};

/// `field` spelled as the record's tables spell it, upper-case: borrowed
/// unless the name has a lower-case letter, so a declared name or an
/// upper-case key is never copied on a post. A by-name entry asks
/// [`RecordCell::canonical_field`], which allocates for no declared name.
pub(crate) fn upper_field(field: &str) -> Cow<'_, str> {
    if field.bytes().any(|b| b.is_ascii_lowercase()) {
        Cow::Owned(field.to_ascii_uppercase())
    } else {
        Cow::Borrowed(field)
    }
}

/// Whether `record_type`'s `.dbd` declares `field` — C `dbFindField` on the
/// type's declared list, which `pvNameLookup` (`dbChannel.c:311-329`) asks
/// before it falls through to the attribute table.
pub(crate) fn declares_field(record_type: &str, field: &str) -> bool {
    super::dbd_generated::record_declaration_order(record_type)
        .is_some_and(|names| names.contains(&field))
}

/// The record's data, guarded by its lock set. `Sync` on exactly the bound
/// `RwLock<T>` demanded for it — the lock set excludes every other thread
/// while a borrow is out, the same guarantee the `RwLock` gave.
struct LockSetGuarded<T: Send + Sync>(std::cell::UnsafeCell<T>);

// SAFETY: a `&T` or `&mut T` is only ever produced by `RecordCell::read` /
// `RecordCell::write`, on a thread that holds the record's lock set, and
// lives no longer than that hold (`RecordRef` / `RecordMut` carry the set
// guard). Two threads therefore never observe the cell at once, and the
// same-thread borrow rules are enforced by `RecordCell::borrow`.
unsafe impl<T: Send + Sync> Sync for LockSetGuarded<T> {}

/// C `processTarget`'s hold of `psrc->pact`: set for the guard's life, then
/// put back as it was (`dbDbLink.c:443`, `:457`, `:514`).
pub(crate) struct ChainPactHold<'a> {
    flag: &'a AtomicBool,
    was: bool,
}

impl Drop for ChainPactHold<'_> {
    #[inline(always)]
    fn drop(&mut self) {
        self.flag.store(self.was, Ordering::Relaxed);
    }
}

impl RecordCell {
    /// Hold this record's PACT as a target's source while the guard lives.
    /// Under the chain's lock set, so a reader that takes it sees C's value.
    #[inline(always)]
    pub(crate) fn hold_chain_pact(&self) -> ChainPactHold<'_> {
        let flag = &self.chain_pact;
        let was = flag.load(Ordering::Relaxed);
        flag.store(true, Ordering::Relaxed);
        ChainPactHold { flag, was }
    }

    /// Record borrows the calling thread holds, for a test binary's
    /// allocator to tell allocations under a hold from the rest; `0` in
    /// builds without debug assertions.
    #[doc(hidden)]
    pub fn live_borrows_on_this_thread() -> usize {
        live_borrows::count()
    }

    /// The cell for `instance`, registered at record-table `index` if given.
    pub(crate) fn new(
        mut instance: RecordInstance,
        load_order: u64,
        init_owed: bool,
        index: Option<crate::server::database::RecordIdx>,
    ) -> Arc<Self> {
        Arc::new_cyclic(|cell| {
            instance.cold.cell = cell.clone();
            let cell = Self::build(instance, load_order, init_owed);
            match index {
                Some(index) => cell.indexed(index),
                None => cell,
            }
        })
    }

    fn build(mut instance: RecordInstance, load_order: u64, init_owed: bool) -> Self {
        let slots: Box<[_]> = instance
            .record
            .field_list()
            .iter()
            .map(|f| instance.record.field_slot(f.name))
            .collect();
        Self {
            lock_record: crate::server::database::LockRecord::bootstrap(),
            borrow: std::sync::atomic::AtomicIsize::new(0),
            cvt_pending: std::sync::atomic::AtomicU64::new(0),
            bkpt: instance.common.bkpt.share(),
            rdes: RecordDesc::of(&*instance.record),
            name: instance.name.clone(),
            put_facts: put_facts(&mut instance, &slots),
            slots,
            multi_input_val_slots: instance
                .record
                .multi_input_links()
                .iter()
                .map(|(_, vf)| instance.record.field_slot(vf))
                .collect(),
            plan: ProcessPlan::of(&*instance.record),
            load_order,
            index: None,
            init_owed: std::sync::atomic::AtomicBool::new(init_owed),
            link_opens_owed: std::sync::atomic::AtomicBool::new(true),
            chain_pact: AtomicBool::new(false),
            string_typed: std::sync::atomic::AtomicU64::new(string_typed_mask(&instance)),
            dbaddr_counts: arc_swap::ArcSwap::from_pointee(dbaddr_counts_of(&instance)),
            metadata: Arc::clone(&instance.cold.metadata_cache),
            data: LockSetGuarded(std::cell::UnsafeCell::new(instance)),
        }
    }

    /// This record's database load sequence, without taking its lock set.
    pub(crate) fn load_order(&self) -> u64 {
        self.load_order
    }

    /// The cell as registered at record-table index `index`.
    fn indexed(mut self, index: crate::server::database::RecordIdx) -> Self {
        self.index = Some(index);
        let body = &*self.data.0.get_mut().record;
        self.lock_record.hints().body.set(
            (body as *const dyn super::Record).cast(),
            std::mem::size_of_val(body),
            std::mem::align_of_val(body),
        );
        self
    }

    /// This record's record-table index; see [`Self::indexed`].
    pub(crate) fn index(&self) -> Option<crate::server::database::RecordIdx> {
        self.index
    }

    /// Is this record's `init_record` still owed to `iocInit`?
    pub(crate) fn init_owed(&self) -> bool {
        self.init_owed.load(std::sync::atomic::Ordering::Acquire)
    }

    /// Claim the owed init: `true` to exactly one caller, which then owns
    /// running it. The single transition out of [`Self::init_owed`], so the
    /// init pass cannot run twice for one record however many times the
    /// barrier is crossed.
    pub(crate) fn claim_owed_init(&self) -> bool {
        self.init_owed
            .swap(false, std::sync::atomic::Ordering::AcqRel)
    }

    /// Claim the init-time `openLink` of this record's links: `true` to
    /// exactly one caller ([`Self::link_opens_owed`]).
    pub(crate) fn claim_link_opens(&self) -> bool {
        self.link_opens_owed
            .swap(false, std::sync::atomic::Ordering::AcqRel)
    }

    /// Re-read [`Self::string_typed`] and [`Self::dbaddr_counts`] from
    /// `inst`, this cell's record, after a load or an `init_record` that may
    /// have moved a type selector or a capacity.
    pub(crate) fn settle_addr_facts(&self, inst: &RecordInstance) {
        self.string_typed.store(
            string_typed_mask(inst),
            std::sync::atomic::Ordering::Relaxed,
        );
        self.dbaddr_counts.store(Arc::new(dbaddr_counts_of(inst)));
    }

    /// C `dbNameToAddr(name, &addr)` on this record succeeding, answered with
    /// `addr.no_elements` (1, or an array field's capacity), with no lock:
    /// the field is resolved against the type's declaration and the capacity
    /// was settled at init. `None` is a field the type does not declare.
    pub fn no_elements(&self, field: &str) -> Option<u32> {
        let named = |f: &&FieldDesc| f.name.eq_ignore_ascii_case(field);
        let Some(i) = self.rdes.fields.iter().position(|f| named(&f)) else {
            return super::dbd_generated::DB_COMMON_FIELDS
                .iter()
                .any(|f| named(&f))
                .then_some(1);
        };
        let counts = self.dbaddr_counts.load();
        Some(
            counts
                .iter()
                .find(|(at, _)| usize::from(*at) == i)
                .map_or(1, |(_, n)| *n),
        )
    }

    /// [`Self::no_elements`] as C's `dbNameToAddr` RUNS it: the record's
    /// `cvt_dbaddr` too, now if the record is free, else owed to the next
    /// fresh hold of its set (`record_lock::owe_cvt`).
    pub(crate) fn resolve_addr(self: &Arc<Self>, field: &str) -> Option<u32> {
        let count = self.no_elements(field)?;
        let counts = self.dbaddr_counts.load();
        let at = counts.iter().position(|(at, _)| {
            self.rdes.fields[usize::from(*at)]
                .name
                .eq_ignore_ascii_case(field)
        });
        if let Some(bit) = at {
            debug_assert!(bit < 64, "{} has over 64 SPC_DBADDR fields", self.name);
            self.cvt_pending
                .fetch_or(1 << bit, std::sync::atomic::Ordering::Release);
            match self.try_read() {
                // SAFETY: `_held` is a shared borrow of this record.
                Some(_held) => unsafe { self.apply_cvt_pending() },
                None => crate::server::database::owe_cvt(self),
            }
        }
        Some(count)
    }

    /// Run the `cvt_dbaddr`s [`Self::resolve_addr`] left owed.
    ///
    /// # Safety
    ///
    /// This thread holds a guard of the record, or holds its lock set with no
    /// borrow of the record live — so no `&mut` to the data exists.
    pub(crate) unsafe fn apply_cvt_pending(&self) {
        let mut owed = self
            .cvt_pending
            .swap(0, std::sync::atomic::Ordering::Acquire);
        if owed == 0 {
            return;
        }
        let counts = self.dbaddr_counts.load();
        // SAFETY: the caller's contract.
        let inst = unsafe { &*self.data.0.get() };
        while owed != 0 {
            let bit = owed.trailing_zeros() as usize;
            owed &= owed - 1;
            let (at, _) = counts[bit];
            inst.record
                .cvt_dbaddr(self.rdes.fields[usize::from(at)].name);
        }
    }

    /// C `dbChannelCreate`'s `$` test (`dbChannel.c:486-503`) on the type
    /// `cvt_dbaddr` leaves (`dbEntryToAddr`, `dbAccess.c:642-648`): a
    /// `DBF_STRING` or a link field.
    pub(crate) fn takes_string_view(&self, desc: &FieldDesc) -> bool {
        use crate::types::DbfCode;
        if crate::types::link_class_of(desc).is_some() {
            return true;
        }
        if !desc.runtime_typed {
            return crate::server::database::char_view::field_addr_types(desc, None).0
                == DbfCode::String;
        }
        let mask = self.string_typed.load(std::sync::atomic::Ordering::Relaxed);
        self.rdes
            .fields
            .iter()
            .filter(|f| f.runtime_typed)
            .position(|f| f.name == desc.name)
            .is_some_and(|i| mask >> i & 1 == 1)
    }

    /// The type-static answers this cycle may test instead of re-asking the
    /// record under a lock.
    pub(crate) fn process_plan(&self) -> &ProcessPlan {
        &self.plan
    }

    /// The record type's declaration — C `precord->rdes`.
    pub(crate) fn rdes(&self) -> &RecordDesc {
        &self.rdes
    }

    /// The field name a by-name entry works with: the declared spelling of
    /// the type's own field or a `dbCommon` one, matched without case (the
    /// port takes the lower-case names C's `dbNameToAddr` refuses), found
    /// with no allocation. Any other name goes to [`upper_field`].
    pub(crate) fn canonical_field<'f>(&self, field: &'f str) -> Cow<'f, str> {
        let named = |f: &&FieldDesc| f.name.eq_ignore_ascii_case(field);
        self.rdes
            .fields
            .iter()
            .find(named)
            .or_else(|| super::dbd_generated::DB_COMMON_FIELDS.iter().find(named))
            .map_or_else(|| upper_field(field), |d| d.name.into())
    }

    /// A put name's field part as C `dbNameToAddr` reads it: the declared
    /// spelling ([`Self::canonical_field`]) and whether a trailing `$` asks
    /// for the field's `DBR_CHAR` address (`dbAccess.c:681-701`).
    pub(crate) fn put_name<'f>(&self, field: &'f str) -> (Cow<'f, str>, bool) {
        match field.strip_suffix('$') {
            Some(core) => (self.canonical_field(core), true),
            None => (self.canonical_field(field), false),
        }
    }

    /// C `dbFindFieldPart`'s answer for `field`: a name the type's `.dbd`
    /// declares, `DBF_NOACCESS` ones included, matched with `strncmp`
    /// (`dbStaticLib.c:1822`). A fact of the type, so it takes no lock.
    ///
    /// A type with no field table (a programmatic record no `.dbd` declares)
    /// has only the spelling to compare against.
    pub(crate) fn declares_name(&self, field: &str) -> bool {
        let rdes = &self.rdes;
        if rdes.fields.is_empty() {
            return crate::server::database::filters::parser::spells_a_field(field);
        }
        declares_field(rdes.record_type, field)
            || rdes
                .fields
                .iter()
                .chain(super::dbd_generated::DB_COMMON_FIELDS)
                .any(|f| f.name == field)
            || is_dbcommon_noaccess(field)
            || rdes.noaccess.contains(&field)
    }

    /// [`FieldDesc::long_string`] of `field`, without taking the lock set.
    pub fn field_is_long_string(&self, field: &str) -> bool {
        super::record_trait::long_string_in(self.rdes.fields, field)
    }

    /// The record's name — C `precord->name` — without taking its lock set.
    pub fn name(&self) -> &crate::types::RecordName {
        &self.name
    }

    /// The slot the fetch stores multi-input link `index`'s value through,
    /// when the type hands one out.
    #[inline(always)]
    pub(crate) fn multi_input_val_slot(
        &self,
        index: usize,
    ) -> Option<super::record_trait::FieldSlot> {
        self.multi_input_val_slots[index]
    }

    /// The record's `BKPT` byte, without taking its lock set.
    pub(crate) fn bkpt(&self) -> &BkptFlag {
        &self.bkpt
    }

    /// Shared access to the record — C `dbScanLock` followed by reading the
    /// fields. Blocks while another thread holds the record's lock set;
    /// recurses on the thread that holds it.
    ///
    /// # Panics
    ///
    /// If this thread holds a [`RecordMut`] of the same record: the borrow
    /// the caller asked for cannot coexist with it, and the `RwLock` this
    /// replaced would have parked the thread forever here.
    pub fn read(&self) -> RecordRef<'_> {
        use std::sync::atomic::Ordering;
        let set = self.lock_record.acquire();
        let borrow = self.borrow.load(Ordering::Relaxed);
        assert!(
            borrow >= 0,
            "record data read while this thread holds it for writing"
        );
        self.borrow.store(borrow + 1, Ordering::Relaxed);
        live_borrows::taken();
        RecordRef {
            cell: self,
            _set: Some(set),
        }
    }

    /// [`Self::read`] without waiting: `None` while another thread holds the
    /// record's lock set, or while this thread holds the record for writing.
    pub(crate) fn try_read(&self) -> Option<RecordRef<'_>> {
        use std::sync::atomic::Ordering;
        let set = self.lock_record.try_acquire()?;
        let borrow = self.borrow.load(Ordering::Relaxed);
        if borrow < 0 {
            return None;
        }
        self.borrow.store(borrow + 1, Ordering::Relaxed);
        live_borrows::taken();
        Some(RecordRef {
            cell: self,
            _set: Some(set),
        })
    }

    /// [`Self::read`] for a thread that holds `held`: when this record is in
    /// that set the set is held already, and only the borrow count is
    /// touched — no thread key, no depth counter. Every DB link target is in
    /// its reader's set (the merge, see `record_lock.rs`), so this is the
    /// per-link read of a process cycle; a record in another set takes
    /// [`Self::read`]'s path. Sound by Rule R: a held set keeps its records,
    /// so the test cannot go stale while `held` lives, and the borrow is
    /// bound to `held` so it cannot outlive the hold.
    ///
    /// # Panics
    ///
    /// As [`Self::read`].
    #[inline(always)]
    pub(crate) fn read_in<'a>(
        &'a self,
        held: &'a crate::server::database::SetGuard,
    ) -> RecordRef<'a> {
        use std::sync::atomic::Ordering;
        if !held.holds(&self.lock_record) {
            return self.read();
        }
        let borrow = self.borrow.load(Ordering::Relaxed);
        assert!(
            borrow >= 0,
            "record data read while this thread holds it for writing"
        );
        self.borrow.store(borrow + 1, Ordering::Relaxed);
        live_borrows::taken();
        RecordRef {
            cell: self,
            _set: None,
        }
    }

    /// [`Self::read_in`] for a record known to be in `held`'s set — a process
    /// frame's own record — so the membership test is a debug assertion only.
    ///
    /// # Panics
    ///
    /// As [`Self::read`]; and, in a debug build, if `held` is another set.
    #[inline(always)]
    pub(crate) fn read_held<'a>(
        &'a self,
        held: &'a crate::server::database::SetGuard,
    ) -> RecordRef<'a> {
        use std::sync::atomic::Ordering;
        debug_assert!(
            held.holds(&self.lock_record),
            "record read under the guard of another lock set"
        );
        let borrow = self.borrow.load(Ordering::Relaxed);
        assert!(
            borrow >= 0,
            "record data read while this thread holds it for writing"
        );
        self.borrow.store(borrow + 1, Ordering::Relaxed);
        live_borrows::taken();
        RecordRef {
            cell: self,
            _set: None,
        }
    }

    /// Exclusive access to the record — C `dbScanLock` followed by writing
    /// the fields. Blocks and recurses as [`Self::read`] does.
    ///
    /// # Panics
    ///
    /// If this thread holds any guard of the same record, for the reason
    /// given on [`Self::read`].
    pub fn write(&self) -> RecordMut<'_> {
        use std::sync::atomic::Ordering;
        let set = self.lock_record.acquire();
        assert!(
            self.borrow.load(Ordering::Relaxed) == 0,
            "record data written while this thread still holds a guard of it"
        );
        self.borrow.store(-1, Ordering::Relaxed);
        live_borrows::taken();
        RecordMut {
            cell: self,
            _set: Some(set),
        }
    }

    /// [`Self::write`] for a sweep over every record, which may run inside
    /// a frame of one: `None` when this thread already borrows the record.
    /// Holding the set proves no other thread does.
    pub(crate) fn try_write(&self) -> Option<RecordMut<'_>> {
        use std::sync::atomic::Ordering;
        let set = self.lock_record.acquire();
        if self.borrow.load(Ordering::Relaxed) != 0 {
            return None;
        }
        self.borrow.store(-1, Ordering::Relaxed);
        live_borrows::taken();
        Some(RecordMut {
            cell: self,
            _set: Some(set),
        })
    }

    /// [`Self::write`] for a put that may run inside a frame: under `held`,
    /// when this record is in that set, only the borrow flag is touched, as
    /// [`Self::write_in`]; any other record takes [`Self::write`]'s acquire.
    /// Sound by Rule R, as [`Self::read_in`].
    ///
    /// # Panics
    ///
    /// As [`Self::write`].
    #[inline(always)]
    pub(crate) fn write_under<'a>(
        &'a self,
        held: Option<&'a crate::server::database::SetGuard>,
    ) -> RecordMut<'a> {
        use std::sync::atomic::Ordering;
        if !held.is_some_and(|held| held.holds(&self.lock_record)) {
            return self.write();
        }
        assert!(
            self.borrow.load(Ordering::Relaxed) == 0,
            "record data written while this thread still holds a guard of it"
        );
        self.borrow.store(-1, Ordering::Relaxed);
        live_borrows::taken();
        RecordMut {
            cell: self,
            _set: None,
        }
    }

    /// [`Self::write`] for a thread that holds `held`, this record's set:
    /// only the borrow flag is touched — no thread key, no depth counter,
    /// and nothing to give back on drop but the flag. The shape of a process
    /// frame, which takes the set once at its entry and never again; the
    /// borrow is bound to `held` so it cannot outlive the hold.
    ///
    /// # Panics
    ///
    /// As [`Self::write`]; and, in a debug build, if `held` is another set.
    #[inline(always)]
    pub(crate) fn write_in<'a>(
        &'a self,
        held: &'a crate::server::database::SetGuard,
    ) -> HeldMut<'a> {
        use std::sync::atomic::Ordering;
        debug_assert!(
            held.holds(&self.lock_record),
            "record written under the guard of another lock set"
        );
        assert!(
            self.borrow.load(Ordering::Relaxed) == 0,
            "record data written while this thread still holds a guard of it"
        );
        self.borrow.store(-1, Ordering::Relaxed);
        live_borrows::taken();
        HeldMut { cell: self, held }
    }

    /// Give up this record's lock set for the duration of `f` — the
    /// breakpoint park. Cold, so it checks what the set cannot see:
    /// [`HeldMut`] and [`Self::read_held`] add no depth, and a borrow of
    /// this record live across the park would be read by another thread.
    ///
    /// # Panics
    ///
    /// If a borrow of this record is live, or as the lock set's `unheld`.
    #[cold]
    pub(crate) fn unheld<R>(&self, f: impl FnOnce() -> R) -> R {
        assert_eq!(
            self.borrow.load(std::sync::atomic::Ordering::Relaxed),
            0,
            "a record was given up to a park with a borrow of it live"
        );
        // A borrow of ANOTHER record in the set crosses the park just as
        // unseen — an outer frame's `HeldMut` of the chain's entry record.
        debug_assert_eq!(
            live_borrows::count(),
            0,
            "a record was given up to a park with a borrow of a record in its set live"
        );
        self.lock_record.unheld(f)
    }

    /// The lock-set cell — C `precord->lset`.
    pub(crate) fn lock_record(&self) -> &std::sync::Arc<crate::server::database::LockRecord> {
        &self.lock_record
    }

    /// The cell's bytes a process cycle reads: the run
    /// `the_cycle_read_fields_stay_one_run` pins, from the instance's
    /// `subscribers` through the cell's `name`.
    const CYCLE_RUN: core::ops::Range<usize> = {
        let start = std::mem::offset_of!(RecordCell, data)
            + std::mem::offset_of!(LockSetGuarded<RecordInstance>, 0)
            + std::mem::offset_of!(RecordInstance, subscribers);
        start..std::mem::offset_of!(RecordCell, name) + size_of::<crate::types::RecordName>()
    };

    /// Hint the lines of the cell's cycle run.
    #[inline(always)]
    pub(crate) fn prefetch_cell(&self) {
        let run = Self::CYCLE_RUN;
        // The run's start is aligned to its own lowest set bit, at most the cell's.
        let aligned = (run.start | align_of::<Self>()).isolate_lowest_one();
        super::prefetch_span(
            (self as *const Self).cast::<u8>().wrapping_add(run.start),
            run.end - run.start,
            aligned,
        );
    }

    /// Hint the cell's `LockRecord` and name — fields outside the instance,
    /// so no hold is needed to follow them.
    #[inline(always)]
    pub(crate) fn prefetch_lock_record(&self) {
        super::prefetch_val(std::sync::Arc::as_ptr(&self.lock_record));
        super::prefetch_val(self.name.as_ptr());
    }

    /// Hint what the `LockRecord` names: the lock set and the cycle's hints.
    #[inline(always)]
    pub(crate) fn prefetch_targets(&self) {
        self.lock_record.prefetch_targets();
    }
}

/// A shared borrow of a record, holding its lock set — what
/// [`RecordCell::read`] hands out. `!Send`, as the set guard inside it is:
/// a lock set is released by the thread that took it.
#[must_use = "the record's lock set is released as soon as the guard is dropped"]
pub struct RecordRef<'a> {
    cell: &'a RecordCell,
    /// `None` for a [`RecordCell::read_in`] borrow, which rides on the set
    /// guard of its `'a` rather than taking one.
    _set: Option<crate::server::database::SetGuard>,
}

impl RecordRef<'_> {
    /// Whether the borrow rode a set guard the caller held rather than
    /// taking one — [`RecordCell::read_in`]'s same-set case.
    #[cfg(test)]
    pub(crate) fn rides_held_set(&self) -> bool {
        self._set.is_none()
    }
}

impl std::ops::Deref for RecordRef<'_> {
    type Target = RecordInstance;

    fn deref(&self) -> &RecordInstance {
        // SAFETY: this thread holds the record's lock set (`_set`) and the
        // borrow count admitted a shared borrow, so no `&mut` exists.
        unsafe { &*self.cell.data.0.get() }
    }
}

impl Drop for RecordRef<'_> {
    fn drop(&mut self) {
        use std::sync::atomic::Ordering;
        // Runs before `_set` is released (fields drop after this body), so
        // the count is never touched by a thread outside the set.
        let borrow = self.cell.borrow.load(Ordering::Relaxed);
        self.cell.borrow.store(borrow - 1, Ordering::Relaxed);
        live_borrows::dropped();
    }
}

/// An exclusive borrow of a record, holding its lock set — what
/// [`RecordCell::write`] hands out.
#[must_use = "the record's lock set is released as soon as the guard is dropped"]
pub struct RecordMut<'a> {
    cell: &'a RecordCell,
    /// `None` for a [`RecordCell::write_under`] borrow inside the caller's
    /// held set.
    _set: Option<crate::server::database::SetGuard>,
}

impl std::ops::Deref for RecordMut<'_> {
    type Target = RecordInstance;

    fn deref(&self) -> &RecordInstance {
        // SAFETY: as `RecordRef`, and the borrow count admitted the one
        // exclusive borrow, so this is the only reference.
        unsafe { &*self.cell.data.0.get() }
    }
}

impl std::ops::DerefMut for RecordMut<'_> {
    fn deref_mut(&mut self) -> &mut RecordInstance {
        // SAFETY: as `deref`, through the unique borrow of the guard.
        unsafe { &mut *self.cell.data.0.get() }
    }
}

/// An exclusive borrow of a record under a set guard the caller holds — what
/// [`RecordCell::write_in`] hands out. Carries no hold of its own.
#[must_use = "the record's borrow flag is cleared as soon as the guard is dropped"]
pub(crate) struct HeldMut<'a> {
    cell: &'a RecordCell,
    held: &'a crate::server::database::SetGuard,
}

impl std::ops::Deref for HeldMut<'_> {
    type Target = RecordInstance;

    fn deref(&self) -> &RecordInstance {
        // SAFETY: as `RecordMut`: this thread holds the record's set (`held`)
        // and the borrow count admitted the one exclusive borrow.
        unsafe { &*self.cell.data.0.get() }
    }
}

impl std::ops::DerefMut for HeldMut<'_> {
    fn deref_mut(&mut self) -> &mut RecordInstance {
        // SAFETY: as `deref`, through the unique borrow of the guard.
        unsafe { &mut *self.cell.data.0.get() }
    }
}

impl<'a> HeldMut<'a> {
    /// The instance and the set guard it is held under, apart.
    #[inline]
    pub(crate) fn split(&mut self) -> (&mut RecordInstance, &'a crate::server::database::SetGuard) {
        // SAFETY: as `deref_mut`; the set guard is a separate borrow.
        (unsafe { &mut *self.cell.data.0.get() }, self.held)
    }
}

impl Drop for HeldMut<'_> {
    fn drop(&mut self) {
        self.cell
            .borrow
            .store(0, std::sync::atomic::Ordering::Relaxed);
        live_borrows::dropped();
    }
}

impl Drop for RecordMut<'_> {
    fn drop(&mut self) {
        self.cell
            .borrow
            .store(0, std::sync::atomic::Ordering::Relaxed);
        live_borrows::dropped();
    }
}

/// One entry of [`RecordInstance::parsed_inputs`]: a link text the record
/// held at some read, its parse, and the local record the parse addressed.
pub(crate) struct ParsedInputLink {
    text: PvString,
    /// The [`Record::input_links_generation`] the record answered when
    /// `text` was last read off it, for a type that answers one: while the
    /// record still answers it, `text` IS the field's text and the cycle
    /// need not read the field to know.
    generation: Option<u64>,
    parsed: Arc<ParsedLink>,
    /// [`Record::input_link_failure_is_inert`] and
    /// [`Record::input_link_request`] for this link, with the
    /// [`Record::input_links_generation`] they were asked at: a record that
    /// answers a generation moves it on every write that can change either,
    /// so an answer stamped with the current generation is still the answer.
    answers: Option<(u64, bool, super::record_trait::InputLinkRequest)>,
    /// C's `dbAddr` from `dbDbInitLink` (`dbDbLink.c:88-111`): the target
    /// record, resolved once and read through until something can have
    /// changed the answer. C re-resolves only when the link text is put; the
    /// port also re-resolves when a name map changed, because records and
    /// aliases can come and go after `iocInit` here.
    target: Option<LinkTargetHandle>,
}

/// A resolved link target and the name-map revision it was resolved under.
///
/// Holds the target's `Arc` — C's `dbAddr` holds the record pointer — so the
/// cycle's read is a load, not a `Weak` upgrade. What keeps that from
/// leaking: a record leaves the name map only through
/// [`RecordInstance::destroy`], which drops the handles the record holds,
/// and the map's revision moves with it so every handle that named the
/// record is re-resolved on its holder's next read; the database's own drop
/// destroys every record still in the map, which breaks the `A -> B -> A`
/// cycles two such handles make.
struct LinkTargetHandle {
    target: ResolvedTarget,
    revision: u64,
    /// Settled with the target, since every input is the parse's or the
    /// target's: whether the reader's own hold reads it, and how.
    native: Option<NativeRead>,
}

/// How the held native fetch reads a link it may read — C's link flags and
/// `dbAddr` as the fetch consumes them, decided once per resolution rather
/// than per cycle.
#[derive(Clone, Copy)]
pub(crate) struct NativeRead {
    /// The field is not `VAL`, so a simple PV may shadow its spelling and
    /// the directory is asked first.
    pub(crate) shadowed: bool,
    /// `MS` / `MSS` / `MSI`: the target's alarm is inherited.
    pub(crate) inherits: bool,
    /// The reader's own slot for the link's value field
    /// ([`RecordCell::multi_input_val_slot`]) — C's `&prec->a + i`, the
    /// address the read is stored through, when the type hands one out.
    pub(crate) val_slot: Option<super::record_trait::FieldSlot>,
}

impl NativeRead {
    /// The read the reader's hold makes of `parsed` at `target`, or `None`
    /// when it is not this frame's: a read of the reader's own field, a `PP`
    /// source (processed with the guard released), a filtered channel, or
    /// not a local record read at all.
    fn of(
        parsed: &ParsedLink,
        target: &ResolvedTarget,
        reader: &Arc<RecordCell>,
        slot: usize,
    ) -> Option<Self> {
        use crate::server::record::{LinkProcessPolicy, MonitorSwitch};
        let ParsedLink::Db(db) = parsed else {
            return None;
        };
        let channel = db.target();
        if Arc::ptr_eq(&target.rec, reader)
            || db.policy == LinkProcessPolicy::ProcessPassive
            || channel.json_suffix.is_some()
        {
            return None;
        }
        Some(NativeRead {
            shadowed: &*channel.field != "VAL",
            inherits: db.monitor_switch != MonitorSwitch::NoMaximize,
            val_slot: reader.multi_input_val_slot(slot),
        })
    }
}

impl ParsedInputLink {
    /// The entry of `cache` for `slot`, holding the parse of the text `record`
    /// has in link `slot` of `declared` (its [`Record::multi_input_links`])
    /// now; `None` when the link is unset. The list rather than the name,
    /// so the frame carries an index and reads the name only when it reads
    /// the text.
    ///
    /// The single writer of [`RecordInstance::parsed_inputs`]: the record's
    /// current text is read first and the cached entry is reused only when
    /// its text is that text byte for byte, so the parse runs once per
    /// distinct text a slot has held and the invariant on the field's doc
    /// needs no other site to hold. Takes the record's parts rather than the
    /// record so the fetch stage can hold the entry while it writes the
    /// record's fields. `always`: the held and the general fetch both call
    /// it, and the held one is the loop's common frame.
    ///
    /// `generation` is [`Record::input_links_generation`] as the caller read
    /// it under the hold it calls this under — never carried across a
    /// release, since a put in the gap moves it. An entry stamped with that
    /// generation was compared byte for byte at it, and the record's one
    /// counting writer has not run since — so the text is not read at all.
    #[inline(always)]
    pub(crate) fn validated<'c>(
        cache: &'c mut [Option<ParsedInputLink>],
        record: &dyn Record,
        slot: usize,
        declared: &'static [(&'static str, &'static str)],
        generation: Option<u64>,
    ) -> Option<&'c mut ParsedInputLink> {
        let cell = cache.get_mut(slot)?;
        if generation.is_some()
            && let Some(entry) = cell.as_ref()
            && entry.generation == generation
        {
            debug_assert!(
                record.link_text_ref(declared[slot].0) == Some(entry.text.as_bytes()),
                "{}: the link text changed under generation {generation:?} — \
                 a writer of the text does not move input_links_generation",
                declared[slot].0
            );
            return cell.as_mut();
        }
        let link_field = declared[slot].0;
        let owned;
        let text: &[u8] = match record.link_text_ref(link_field) {
            Some([]) => return None,
            Some(text) => text,
            None => {
                owned = link_text_of(record, link_field)?;
                owned.as_bytes()
            }
        };
        if cell.as_ref().is_none_or(|entry| entry.text != text) {
            *cell = Some(Self::fresh(text));
        }
        let entry = cell.as_mut()?;
        entry.generation = generation;
        Some(entry)
    }

    /// [`RecordInstance::bind_no_lset`]'s entry: `text` held with no lset.
    fn unbound(text: crate::types::PvString, removed: Arc<ParsedLink>) -> ParsedInputLink {
        ParsedInputLink {
            text,
            generation: None,
            parsed: removed,
            answers: None,
            target: None,
        }
    }

    /// A slot's entry for a text it has not held: the parse, with no target
    /// resolved yet. Its own frame so the cycle's byte-for-byte reuse test
    /// does not carry the parser's.
    fn fresh(text: &[u8]) -> ParsedInputLink {
        ParsedInputLink {
            text: text.into(),
            generation: None,
            parsed: Arc::new(parse_link_v2(text)),
            answers: None,
            target: None,
        }
    }

    /// What the record answers for this link's failure and request: asked of
    /// the record once per [`Record::input_links_generation`] it answers, and
    /// every time for a record that answers none.
    #[inline(always)]
    pub(crate) fn link_answers(
        &mut self,
        record: &dyn Record,
        link_field: &str,
        generation: Option<u64>,
    ) -> (bool, super::record_trait::InputLinkRequest) {
        let ask = || {
            (
                record.input_link_failure_is_inert(link_field),
                record.input_link_request(link_field),
            )
        };
        let Some(generation) = generation else {
            return ask();
        };
        if let Some((at, inert, request)) = self.answers
            && at == generation
        {
            debug_assert!(
                (inert, request) == ask(),
                "{link_field}: an input_link answer changed under generation {generation} — \
                 a writer of what it reads does not move input_links_generation"
            );
            return (inert, request);
        }
        let (inert, request) = ask();
        self.answers = Some((generation, inert, request));
        (inert, request)
    }

    /// The parse, shared: the cycle hands it to reads that run after the
    /// record lock is released.
    pub(crate) fn parsed(&self) -> &Arc<ParsedLink> {
        &self.parsed
    }

    /// The handle at the name maps' current revision: reused while the maps
    /// are at the revision it was resolved under, re-resolved otherwise. A
    /// link that resolves to no local record is asked again on every read,
    /// as it was before the cache. `reader` is the record holding the entry
    /// and `slot` the entry's index in its list, for [`NativeRead::of`].
    ///
    /// Over the entry's parts, so a caller can hold the parse beside the
    /// handle it returns.
    #[inline(always)]
    fn resolve<'h>(
        handle: &'h mut Option<LinkTargetHandle>,
        parsed: &ParsedLink,
        names: &dyn LinkTargetResolver,
        reader: &Arc<RecordCell>,
        slot: usize,
    ) -> Option<&'h LinkTargetHandle> {
        let revision = names.name_revision();
        if handle
            .as_ref()
            .is_none_or(|handle| handle.revision != revision)
        {
            *handle = names.local_target(parsed).map(|target| {
                let native = NativeRead::of(parsed, &target, reader, slot);
                LinkTargetHandle {
                    target,
                    revision,
                    native,
                }
            });
        }
        handle.as_ref()
    }

    /// The parse, with C's `dbAddr` for it — the local record and field the
    /// link addresses, or `None` for a link that is not a local record read.
    /// The two come out together because both borrow the entry and the
    /// cycle wants both of every link.
    #[inline(always)]
    pub(crate) fn target(
        &mut self,
        names: &dyn LinkTargetResolver,
        reader: &Arc<RecordCell>,
        slot: usize,
    ) -> (&ParsedLink, Option<&ResolvedTarget>) {
        let target = Self::resolve(&mut self.target, &self.parsed, names, reader, slot)
            .map(|handle| &handle.target);
        (&self.parsed, target)
    }

    /// The link as the reader's own hold reads it — see [`NativeRead::of`]
    /// — or `None` for a link that is not that frame's.
    #[inline(always)]
    pub(crate) fn native_read(
        &mut self,
        names: &dyn LinkTargetResolver,
        reader: &Arc<RecordCell>,
        slot: usize,
    ) -> Option<(&crate::server::record::DbLink, &ResolvedTarget, NativeRead)> {
        let handle = Self::resolve(&mut self.target, &self.parsed, names, reader, slot)?;
        let native = handle.native?;
        let ParsedLink::Db(db) = &*self.parsed else {
            return None;
        };
        Some((db, &handle.target, native))
    }

    /// Drop the target handle, leaving the parse. What
    /// [`RecordInstance::destroy`] does to every entry, so a record that has
    /// left the database holds no other record.
    fn release_target(&mut self) {
        self.target = None;
    }

    /// Whether the handle names `cell`.
    fn targets(&self, cell: &Arc<RecordCell>) -> bool {
        self.target
            .as_ref()
            .is_some_and(|handle| Arc::ptr_eq(&handle.target.rec, cell))
    }
}

/// Which of the SIM block's two links [`RecordInstance::sim_link`] serves.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum SimLink {
    Siml,
    Siol,
}

impl SimLink {
    fn cycle_link(self) -> CycleLink {
        match self {
            SimLink::Siml => CycleLink::Siml,
            SimLink::Siol => CycleLink::Siol,
        }
    }
}

/// The text of link field `field` of `record`, `None` when the link is unset
/// — [`RecordInstance::link_text`] over the record alone.
fn link_text_of(record: &dyn Record, field: &str) -> Option<PvString> {
    if let Some(text) = record.link_text_ref(field) {
        return (!text.is_empty()).then(|| text.into());
    }
    match record.get_field(field)? {
        EpicsValue::String(text) if !text.is_empty() => Some(text),
        _ => None,
    }
}

/// A link's local target with its field resolved — C's `dbAddr`.
#[derive(Clone)]
pub(crate) struct ResolvedTarget {
    pub(crate) rec: Arc<RecordCell>,
    /// The record's canonical name — the link may name an alias.
    pub(crate) name: crate::types::RecordName,
    /// The field, with the answers a `dbPut` through the link asks.
    pub(crate) put: PutAddr,
    /// The slot a `DBR_DOUBLE` read takes straight off the record: the link
    /// addresses an unfiltered `VAL` the record serves as `DBF_DOUBLE`.
    pub(crate) f64_slot: Option<super::record_trait::FieldSlot>,
}

/// Where a DB link's local target comes from — `PvDatabase` in production.
pub(crate) trait LinkTargetResolver {
    /// The revision of the name maps (records and aliases). Loaded BEFORE
    /// the maps are read, so a handle stamped with it can never be one a
    /// later mutation produced under an earlier number.
    fn name_revision(&self) -> u64;
    /// The local record a `Db` link addresses, with its field resolved, or
    /// `None` for a link that is not a local record read (a constant, an
    /// external PV, a simple PV).
    fn local_target(&self, link: &ParsedLink) -> Option<ResolvedTarget>;
}

/// A field's index in C's `papFldDes` order — `dbCommon`, then the record's
/// own table — taken from its `FieldDesc` by address. The published log's key
/// and the walk's emission order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FieldIx(pub u16);

impl FieldIx {
    /// A field nothing declares (a virtual field, an undeclared type).
    pub const UNDECLARED: Self = Self(u16::MAX);

    fn of(record: &dyn Record, desc: Option<&'static FieldDesc>) -> Self {
        Self::in_table(record.field_list(), desc)
    }

    /// [`Self::of`] against the type's own table `own`.
    fn in_table(own: &'static [FieldDesc], desc: Option<&'static FieldDesc>) -> Self {
        let Some(desc) = desc else {
            return Self::UNDECLARED;
        };
        let common = super::dbd_generated::DB_COMMON_FIELDS;
        let at = |table: &'static [FieldDesc]| {
            let at = desc as *const FieldDesc;
            table
                .as_ptr_range()
                .contains(&at)
                .then(|| (at as usize - table.as_ptr() as usize) / std::mem::size_of::<FieldDesc>())
        };
        at(common)
            .or_else(|| at(own).map(|i| common.len() + i))
            .and_then(|i| u16::try_from(i).ok())
            .map_or(Self::UNDECLARED, Self)
    }
}

/// A set of a type's declared fields by [`FieldIx`], built with the plan so a
/// cycle tests a bit where it would search a name list. Interned by content,
/// so the records of a type share one copy and a cycle's test reads a line
/// every record keeps warm, not one of its own.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct FieldBits(&'static [u64]);

impl FieldBits {
    fn of(record: &dyn Record, mut has: impl FnMut(&'static FieldDesc) -> bool) -> Self {
        static INTERNED: std::sync::Mutex<std::collections::BTreeSet<&'static [u64]>> =
            std::sync::Mutex::new(std::collections::BTreeSet::new());
        let common = super::dbd_generated::DB_COMMON_FIELDS;
        let own = record.field_list();
        let mut words = vec![0u64; (common.len() + own.len()).div_ceil(64)];
        for (i, desc) in common.iter().chain(own).enumerate() {
            if has(desc) {
                words[i / 64] |= 1 << (i % 64);
            }
        }
        let mut interned = INTERNED.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(&bits) = interned.get(words.as_slice()) {
            return Self(bits);
        }
        let bits: &'static [u64] = Box::leak(words.into_boxed_slice());
        interned.insert(bits);
        Self(bits)
    }

    #[inline(always)]
    fn has(&self, ix: FieldIx) -> bool {
        let i = usize::from(ix.0);
        self.0
            .get(i / 64)
            .is_some_and(|word| word >> (i % 64) & 1 != 0)
    }
}

/// The name a subscriber attached under: the field's own [`FieldDesc`] when
/// the spelling is the declared one, the attached spelling otherwise.
#[derive(Debug, Clone)]
enum SubName {
    Declared(&'static FieldDesc),
    Named(Box<str>),
}

/// One monitor on one field of the record — C's `evSubscrip` in `mlis`.
pub struct FieldSubscriber {
    name: SubName,
    pub ix: FieldIx,
    /// [`RecordInstance::declared_field_type`] and
    /// [`RecordCell::field_is_long_string`] of the field, both static,
    /// so a post projects its log without a name lookup.
    declared: Option<DbFieldType>,
    long_string: bool,
    /// What the monitor walk does with this monitor's field, and the field's
    /// membership in the last settled [`Record::process_posted_fields`] —
    /// settled by [`RecordInstance::settle_sub_walk`], so the walk names no
    /// field by its bytes.
    walk: SubWalk,
    posted: bool,
    /// The field's place in [`MonitorPlan::claimed`], `u16::MAX` outside it.
    claimed_rank: u16,
    /// The walk's per-field answers for this monitor's field, settled as a
    /// published entry's are.
    roles: Roles,
    sub: Subscriber,
}

/// How [`RecordInstance::collect_subscriber_posts`] reaches a monitor's field
/// through the monitor rather than through `last_posted`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SubWalk {
    /// Not through the monitor: the field is published (walked from
    /// `last_posted`), always skipped ([`walk_skip_term`]), or an earlier
    /// monitor watches it.
    Skip,
    /// [`MonitorPlan::claimed`]: never published, posted only when owed.
    Claimed,
    /// Not yet published.
    Unpublished,
}

impl FieldSubscriber {
    /// The monitor itself.
    pub fn sub(&self) -> &Subscriber {
        &self.sub
    }

    /// The field this monitor watches, as attached.
    pub fn field(&self) -> &str {
        match &self.name {
            SubName::Declared(desc) => desc.name,
            SubName::Named(name) => name,
        }
    }

    /// The index a declared-field set holds this monitor's field by; a
    /// spelling no declaration matches is in none.
    fn member(&self) -> Option<FieldIx> {
        match &self.name {
            SubName::Declared(_) => Some(self.ix),
            SubName::Named(_) => None,
        }
    }

    /// The field's published-log key.
    fn key(&self) -> PostKey<'_> {
        match &self.name {
            SubName::Declared(_) => PostKey::Ix(self.ix),
            SubName::Named(name) => PostKey::Named(name),
        }
    }

    /// The one rule a post finds its monitors by. Two declared fields are one
    /// pointer or two fields; only an undeclared side reads bytes.
    #[inline(always)]
    fn watches(&self, post: &PostName) -> bool {
        match (&self.name, post) {
            (SubName::Declared(own), PostName::Declared(posted)) => std::ptr::eq(*own, *posted),
            _ => self.watches_str(post),
        }
    }

    /// [`Self::watches`] for a post filed under `key`.
    #[inline(always)]
    fn watches_key(&self, key: PostKey<'_>) -> bool {
        match (&self.name, key) {
            (SubName::Declared(_), PostKey::Ix(ix)) => self.ix == ix,
            (_, PostKey::Named(name)) => self.watches_str(name),
            (SubName::Named(_), PostKey::Ix(_)) => false,
        }
    }

    /// [`Self::watches`] between two monitors.
    fn watches_as(&self, other: &FieldSubscriber) -> bool {
        match (&self.name, &other.name) {
            (SubName::Declared(own), SubName::Declared(theirs)) => std::ptr::eq(*own, *theirs),
            _ => self.watches_str(other.field()),
        }
    }

    /// [`RecordInstance::project_to_declared_type`] of a borrowed `value` of
    /// the watched field, copying it only when the projection changes it: a
    /// post whose log reads a wide field by reference must not copy it to
    /// find that out.
    fn declared_view<'v>(&self, value: &'v EpicsValue) -> Cow<'v, EpicsValue> {
        match declared_projection_of(self.declared, value, || self.long_string) {
            Some(declared) => Cow::Owned(value.clone().into_type(declared)),
            None => Cow::Borrowed(value),
        }
    }

    /// [`Self::watches`] for a name of unknown provenance: the pointer first,
    /// then the bytes. Counted on entry: a name that happens to share the
    /// declared pointer is still a by-name caller.
    #[inline(always)]
    fn watches_str(&self, field: &str) -> bool {
        #[cfg(test)]
        NAME_BYTE_COMPARES.with(|n| n.set(n.get() + 1));
        let own = self.field();
        std::ptr::eq(own, field) || name_bytes_eq(own, field)
    }
}

/// Two field names by their bytes, the fallback when neither side carries
/// its declared pointer.
#[inline(always)]
fn name_bytes_eq(a: &str, b: &str) -> bool {
    #[cfg(test)]
    NAME_BYTE_COMPARES.with(|n| n.set(n.get() + 1));
    a == b
}

/// Whether `list` holds the field at `member`.
fn holds(list: DeclaredFields, member: Option<FieldIx>) -> bool {
    member.is_some_and(|member| list.contains(&member))
}

/// Whether a declared name list holds `field`, by bytes.
fn names_contain(list: &[&str], field: &str) -> bool {
    list.iter().any(|name| name_bytes_eq(name, field))
}

#[cfg(test)]
thread_local! {
    /// How many times a post read field-name bytes or resolved a field by
    /// name ([`name_bytes_eq`], [`RecordInstance::walk_skips`],
    /// [`RecordInstance::post_key`]).
    static NAME_BYTE_COMPARES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    /// How many snapshots reached the delivery loop.
    static DELIVERIES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    /// How many times [`RecordInstance::build_metadata`] ran.
    static METADATA_BUILDS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    /// How many log heads were built.
    static HEADS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    /// How many times the subscriber set was tested ([`RecordInstance::unwatched`]).
    static SUBSCRIBER_TESTS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    /// How many cycles entered [`RecordInstance::collect_subscriber_posts`]' walk.
    static WALKS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    /// How many times the record's secondary block was taken.
    static SECONDARY_TAKES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    /// How many fired secondaries [`RecordInstance::post_forced_secondaries`]
    /// went on to look up.
    static FORCED_SECONDARY_VISITS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    /// How many field values were read by name
    /// ([`RecordInstance::resolve_field_stored_at`]).
    static NAME_READS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    /// The fields whose post reached [`RecordInstance::record_value_post_as`]
    /// only to store nothing.
    static STORELESS_POSTS: std::cell::RefCell<Vec<String>> = const { std::cell::RefCell::new(Vec::new()) };
}

/// [`METADATA_BUILDS`] on this thread.
#[cfg(test)]
fn builds() -> usize {
    METADATA_BUILDS.with(std::cell::Cell::get)
}

/// The `dbCommon` declarations the acknowledge put path posts, so it reads
/// no name.
mod common_desc {
    use crate::server::record::{
        FieldDesc, dbd_generated::DB_COMMON_FIELDS as COMMON, declared_field_ix,
    };
    // Indexed at run time: a `const` reference would point at a const-eval
    // copy, which `FieldIx::in_table` cannot place in the table.
    const ACKT_IX: usize = declared_field_ix(&[], "ACKT").0 as usize;
    const ACKS_IX: usize = declared_field_ix(&[], "ACKS").0 as usize;
    pub(super) fn ackt() -> &'static FieldDesc {
        &COMMON[ACKT_IX]
    }
    pub(super) fn acks() -> &'static FieldDesc {
        &COMMON[ACKS_IX]
    }
}

/// A field [`RecordInstance::get_common_field`] answers from
/// [`CommonFields`], so a read addressed by index reaches the same arm as one
/// addressed by name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CommonField {
    Sevr,
    Stat,
    Nsev,
    Nsta,
    Amsg,
    Namsg,
    Acks,
    Ackt,
    Udf,
    Udfs,
    Scan,
    Sscn,
    Oldsimm,
    Pini,
    Tpro,
    Bkpt,
    Flnk,
    Inp,
    Out,
    Dtyp,
    Tse,
    Tsel,
    Utag,
    Asg,
    Asl,
    Desc,
    Phas,
    Evnt,
    Prio,
    Disv,
    Disa,
    Sdis,
    Diss,
    Hyst,
    Lcnt,
    Disp,
    Putf,
    Rpro,
    Pact,
    Proc,
    Hihi,
    High,
    Low,
    Lolo,
    Hhsv,
    Hsv,
    Lsv,
    Llsv,
    Outn,
}

impl CommonField {
    fn by_name(name: &str) -> Option<Self> {
        Some(match name {
            "SEVR" => Self::Sevr,
            "STAT" => Self::Stat,
            "NSEV" => Self::Nsev,
            "NSTA" => Self::Nsta,
            "AMSG" => Self::Amsg,
            "NAMSG" => Self::Namsg,
            "ACKS" => Self::Acks,
            "ACKT" => Self::Ackt,
            "UDF" => Self::Udf,
            "UDFS" => Self::Udfs,
            "SCAN" => Self::Scan,
            "SSCN" => Self::Sscn,
            "OLDSIMM" => Self::Oldsimm,
            "PINI" => Self::Pini,
            "TPRO" => Self::Tpro,
            "BKPT" => Self::Bkpt,
            "FLNK" => Self::Flnk,
            "INP" => Self::Inp,
            "OUT" => Self::Out,
            "DTYP" => Self::Dtyp,
            "TSE" => Self::Tse,
            "TSEL" => Self::Tsel,
            "UTAG" => Self::Utag,
            "ASG" => Self::Asg,
            "ASL" => Self::Asl,
            "DESC" => Self::Desc,
            "PHAS" => Self::Phas,
            "EVNT" => Self::Evnt,
            "PRIO" => Self::Prio,
            "DISV" => Self::Disv,
            "DISA" => Self::Disa,
            "SDIS" => Self::Sdis,
            "DISS" => Self::Diss,
            "HYST" => Self::Hyst,
            "LCNT" => Self::Lcnt,
            "DISP" => Self::Disp,
            "PUTF" => Self::Putf,
            "RPRO" => Self::Rpro,
            "PACT" => Self::Pact,
            "PROC" => Self::Proc,
            "HIHI" => Self::Hihi,
            "HIGH" => Self::High,
            "LOW" => Self::Low,
            "LOLO" => Self::Lolo,
            "HHSV" => Self::Hhsv,
            "HSV" => Self::Hsv,
            "LSV" => Self::Lsv,
            "LLSV" => Self::Llsv,
            "OUTN" => Self::Outn,
            _ => return None,
        })
    }
}

/// How a declared field is read, settled by name once when the plan is built,
/// so a read that carries the declaration matches no name.
#[derive(Debug, Clone, Copy)]
enum FieldRead {
    /// The type's slot, which reads as its `get_field` does.
    Slot(u16),
    /// A [`CommonFields`] arm, for a field the type's `get_field` leaves to
    /// the instance.
    Common(CommonField),
    /// The name chain.
    Named,
    /// C `dbGet`'s refused read ([`FieldDesc::unreadable`]).
    Unreadable,
}

impl FieldRead {
    fn table(record: &dyn Record) -> Box<[FieldRead]> {
        let common = crate::server::record::dbd_generated::DB_COMMON_FIELDS;
        let own = record.field_list();
        let slot = |i: usize| {
            let slot = u16::try_from(i.checked_sub(common.len())?).ok()?;
            record
                .slot_ref(crate::server::record::FieldSlot(slot))
                .map(|_| Self::Slot(slot))
        };
        common
            .iter()
            .chain(own)
            .enumerate()
            .map(|(i, desc)| {
                if desc.unreadable() {
                    return Self::Unreadable;
                }
                // A runtime-typed field's variant is the record's answer at
                // each read (mbbo's VAL turns Enum once SDEF is set).
                if desc.runtime_typed {
                    return Self::Named;
                }
                slot(i).unwrap_or_else(|| match CommonField::by_name(desc.name) {
                    Some(field) if record.get_field(desc.name).is_none() => Self::Common(field),
                    _ => Self::Named,
                })
            })
            .collect()
    }
}

/// A type-erased record instance stored in the database.
///
/// `repr(C)`, fields no process cycle reads first: from `subscribers` on, the
/// fields an unmonitored cycle reads follow in the order it reads them, so that
/// with [`RecordCell`]'s own hot fields right behind they fill the fewest
/// contiguous cache lines. `subscribers` leads that run because only its
/// length, at its end, is read with no monitor attached.
#[repr(C)]
pub struct RecordInstance {
    /// The parsed form of each [`Record::multi_input_links`] slot the record
    /// has wired, validated against the text the record holds NOW on every
    /// read — see [`ParsedInputLink::validated`], the one writer. The record's
    /// put paths need no hook: an entry whose text is not the record's text
    /// is re-parsed at the next read, so `parsed == parse_link_v2(&text)`
    /// holds for every entry by construction. C parses a link once at
    /// `dbInitLink`; the port parsed every set `INPA`..`INPL` on every cycle.
    pub(crate) parsed_inputs: Vec<Option<ParsedInputLink>>,
    /// Every other link field's parse — see [`Self::record_link`].
    link_slots: LinkSlots,
    /// The rows of `link_slots` the multi-output dispatch reads by index
    /// ([`crate::server::database::multi_out_rows`]): the output row, and
    /// seq's DOL row.
    link_rows: (Option<LinkRow>, Option<LinkRow>),
    // Device support
    pub device: Option<Box<dyn super::super::device_support::DeviceSupport>>,
    /// The record's device-write completion, which carries the lock-free copy
    /// of `PRIO`: [`Self::set_prio`] is the one store of both.
    pub(crate) device_done: crate::server::database::DeviceDone,
    /// C `prec->dpvt` for the soft dsets; see [`Dpvt`](crate::server::device_support::Dpvt).
    pub dpvt: crate::server::device_support::Dpvt,
    // Subroutine (for sub records)
    pub subroutine: Option<Arc<Subroutine>>,
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
    /// The fields no process cycle reads; see [`RecordCold`].
    cold: Box<RecordCold>,
    /// Test knob: run every cycle as [`CyclePlan::General`], the reference a
    /// plain arm is compared against.
    #[cfg(test)]
    pub(crate) plan_forced_general: bool,
    #[cfg(test)]
    ran_arm: CyclePlan,
    /// The [`CycleLink`]s' slots in `link_slots` — see [`Self::cycle_link`].
    cycle_links: CycleLinkSlots,
    /// `SELM` menu index (sel), kept the same way for the NVL gate.
    selm: i16,
    /// What this record's dset `init_record` did; written only by
    /// [`crate::server::device_support::init_device_support`].
    pub(crate) dset_init: crate::server::device_support::DsetInit,
    /// C `prec->simpvt`: the callback a simulating record allocates the first
    /// time it defers on `SDLY >= 0` (`aiRecord.c:501-504`, one per SIMM
    /// type) and never frees. Only `dbpr` reads it.
    pub(crate) simpvt: bool,
    /// One-shot "skip the registered subroutine this cycle" signal for aSub
    /// `LFLG=READ`. The async processing path resolves the `SUBL` link before
    /// taking this lock; when the resolved name is bad (C `fetch_values` ->
    /// `S_db_BadSub`) or the link read failed, C `process` runs `do_sub` only
    /// on `!status`, so the subroutine is skipped. Set by the resolution
    /// apply, consumed (and cleared) by [`Self::run_registered_subroutine`];
    /// `false` for every record without a pending bad re-resolution.
    suppress_subroutine_run: bool,
    /// Every monitor on the record in attach order — C's `mlis`, which
    /// `db_post_events` walks matching the field. Two sit inline; a third
    /// grows the buffer under the hold at attach, never on a post.
    pub(crate) subscribers: smallvec::SmallVec<[FieldSubscriber; 2]>,
    pub common: CommonFields,
    pub record: Box<dyn Record>,
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
    /// C `dbCommon::name`: bytes — see [`RecordName`](crate::types::RecordName).
    pub name: crate::types::RecordName,
    /// What this record's TYPE can reach in a process cycle, settled once at
    /// construction. See [`ProcessPlan`].
    /// C's `UDF_ALARM` guard for this type, or `None` for a type whose C
    /// support has no such line (`swait`, `waveform` proper). See
    /// [`UdfAlarm`]. Read by [`Self::evaluate_alarms`] under the lock, which
    /// is why it lives here and not on the cell's [`ProcessPlan`].
    udf_alarm: Option<UdfAlarm>,
    /// The plan resolved at `cycle_plan.0`; stale when that is not `plan_rev`.
    cycle_plan: (u32, CyclePlan, CycleFacts),
    /// Revision of everything [`CyclePlan`] was resolved from. Bumped only by
    /// [`Self::invalidate_plan`].
    plan_rev: u32,
    /// SIMM as it stood before this cycle's SIML read (`rec_gbl_get_simm`), for
    /// an output's stamp, which C takes before `writeValue` reads SIML. `None`
    /// while no SIML read has run since the SIML was last put.
    pub(crate) simm_before_siml_read: Option<i16>,
    /// `(OMSL, OIF)` menu indices, 0 for a type without the field; settled at
    /// construction and by the two put funnels, so the cycle's closed-loop
    /// DOL test reads no field by name.
    dol_mode: (i16, i16),
    /// Terminal destruction marker, the [`crate::server::pv::ProcessVariable`]
    /// flag's counterpart for a record-backed channel. Set once by
    /// [`Self::destroy`], whose only caller is
    /// [`crate::server::database::PvDatabase::remove_record`], so *removed
    /// from the database* and *destroyed* are one event for both target
    /// kinds and a server can sweep them with one uniform test.
    destroyed: bool,
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
    /// Set by `check_deadband_ext` for waveform/aai/aao when their
    /// content hash changed this cycle (C `monitor()` On Change mode,
    /// waveformRecord.c:310-319). The snapshot builders read it to post
    /// `HASH` with a literal `DBE_VALUE` event, independent of the VAL
    /// post mask. False for every record without the MPST/APST/HASH
    /// mechanism.
    pub(crate) array_hash_changed: bool,
    pub parsed_sdis: LinkCache,
    // Link parse cache
    pub parsed_inp: LinkCache,
    pub parsed_out: OutLinkCache,
    pub(crate) parsed_tsel: LinkCache,
    pub parsed_flnk: LinkCache,
    /// See [`MonitorHot`]. Last of the unmonitored run: past it, only a
    /// watched cycle reads on.
    monitor_hot: MonitorHot,
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
    /// posts, whether or not that field has a subscriber (C's `monitor()`
    /// state advances on a post to an empty `mlis`, and a later subscriber
    /// inherits it); an alarm-only / property-only post MUST NOT (those classes do
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
    last_posted: PostedLog,
    /// Owner of every `last_posted` entry's `posted`; see [`PostedSet`].
    posted: PostedSet,
    /// What this record's TYPE answers to `monitor()`'s type-static
    /// questions, settled once at construction. See [`MonitorPlan`].
    monitor_plan: MonitorPlan,
}

/// [`RecordInstance`]'s fields no process cycle reads, behind one pointer so
/// the lines a cycle does read sit closer together.
struct RecordCold {
    /// The cell holding this instance — none for one not yet in a cell — for
    /// the PACT field's read of [`RecordCell::hold_chain_pact`]'s flag.
    cell: std::sync::Weak<RecordCell>,
    /// The by-name function registry this record's PENDING `init_record` pass
    /// 1 will resolve INAM/SNAM against — C `registryFunctionFind`, reading a
    /// process-global table from inside `init_record`.
    ///
    /// One meaning on every path: armed by the init drain immediately
    /// before [`RecordInstance::run_init_pass0`], consumed (and cleared) by
    /// pass 1 itself. It exists because the lookup's failure is an EARLY RETURN — the
    /// init tail must not run past it — and only the init owner can honour
    /// that, while only the database holds the registry. Resolving from
    /// outside the passes, as both builders used to, put the lookup after the
    /// tail it is supposed to skip.
    init_subroutines: Option<Arc<HashMap<String, Arc<Subroutine>>>>,
    /// The database a [`Subroutine::Context`] routine reaches; set at
    /// creation beside `device_done`.
    db: crate::server::database::RecordDb,
    /// The task delivering the bound device's I/O Intr wakes, wired only by
    /// [`RecordInstance::wire_io_intr_pump`].
    io_intr_pump: Option<crate::runtime::background::AbortHandle>,
    /// The task posting the bound device's PROPERTY changes, wired only by
    /// [`RecordInstance::wire_property_posts`].
    property_drain: Option<crate::runtime::background::AbortHandle>,
    /// What a [`Subroutine::Context`] routine stored as C `prec->dpvt`. Not
    /// a [`Dpvt`](crate::server::device_support::Dpvt) variant: a boxed `dyn`
    /// there makes every soft-input read's `Dpvt` overwrite an outlined drop.
    subroutine_pvt: Option<Box<dyn std::any::Any + Send + Sync>>,
    /// Published values for the fields [`RecordInstance::collect_subscriber_posts`]'s
    /// change-detection walk SKIPS whatever happens — the deadband field, the
    /// `recGblResetAlarms` set, `UDF`, and the record's event-posted fields.
    /// They are not walk input, so they must not be in `last_posted`: the
    /// walk's gate asks `last_posted.is_empty()`, and the first alarm
    /// transition put SEVR/STAT/AMSG/ACKS there, so every later cycle of every
    /// record ran the whole walk — six vtable calls and one hash probe plus a
    /// name compare per field — to iterate exactly the four fields it then
    /// discarded. Same owner and same read accessor as `last_posted`; the two
    /// differ only in whether the walk can act on the field, which is what the
    /// gate asks. Linear, because the set is the static skip list.
    skipped_posted: SkippedLog,
    /// The live store for a field the record's `.dbd` DECLARES but the record
    /// struct has no `put_field` arm / no storage for — the WRITE analog of the
    /// read-side [`RecordInstance::declared_default`] fallback.
    ///
    /// C makes every `.dbd` field not just readable but WRITABLE: `dbPutField`
    /// resolves the field from its `dbFldDes` and `dbPut` writes the incoming
    /// value into record memory, whether or not any record code ever reads it
    /// back — a `caput dfanout.HOPR 10` sticks even though `dfanoutRecord.c`
    /// never touches HOPR. A Rust record models only the fields it has
    /// behaviour for, so a field it declares but never stores had nowhere for a
    /// put to land: [`RecordInstance::put_common_field`]'s catch-all reported
    /// `S_dbLib_fieldNotFound` and the client's put was refused, while a READ of
    /// the same field succeeded through `declared_default`. This map is that
    /// missing storage — one uniform mechanism for the whole family, not a
    /// per-field struct member on each record type.
    ///
    /// Keyed by upper-case field name, holding the value already coerced to the
    /// field's C-declared DBF type (the same projection `declared_default` and
    /// the read path serve). [`RecordInstance::resolve_field`] reads it BEFORE
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
    /// Where each of those links sits in [`Record::multi_input_links`] — the
    /// list a process cycle reads once at its top. Both lists are properties
    /// of the record TYPE, so the mapping is fixed here and cannot drift;
    /// `None` marks a metadata link the multi-input fetch does not cover,
    /// which the cycle then reads for itself. Taken once because the
    /// alternative is searching the pre-read list by name, per link, per pass.
    link_backed_metadata_input_slot: Vec<Option<usize>>,
    /// Per-record info tags from `info("key", "value")` directives in
    /// the .db file (epics-base info(...) grammar). Consumers include
    /// asyn (`asyn:READBACK`), record-as-PV bridge tags
    /// (`Q:group`, `Q:form`), and IOC-specific extensions. Empty for
    /// records loaded without info(...) clauses.
    info: HashMap<String, String>,
    /// Cached metadata (display/control/enums) — `None` until built. Seeded
    /// at the end of `init_record` and rebuilt by
    /// `RecordInstance::refresh_metadata_cache`, its only writer, whenever a
    /// metadata-class field (EGU/PREC/HOPR/LOPR/limit/state) is written.
    ///
    /// An `ArcSwapOption` so the cache can be rebuilt from a `&self` method
    /// and read without the record's lock.
    ///
    /// # Cache invariant (CONTRACT)
    ///
    /// The cache is **only correct under the following contract**: every
    /// code path that mutates a cache-source field (the set defined in
    /// the file-private [`is_metadata_cache_source`] predicate) MUST call
    /// [`RecordInstance::notify_field_written`] (or
    /// [`RecordInstance::refresh_metadata_cache`] directly) afterward.
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
    /// next legitimate cache-source write triggers a rebuild.
    ///
    /// # Symmetric note for `populate_*` extensions
    ///
    /// If a future change adds a new field to `populate_display_info`,
    /// `populate_control_info`, or `populate_enum_info`, the new source
    /// field name MUST also be added to [`is_metadata_cache_source`] so
    /// writes to it rebuild the cache — unless, like DESC
    /// (`display.description`), its write owner rebuilds directly (see
    /// the DESC arm of `put_common_field`). This set says nothing about
    /// `DBE_PROPERTY`, which the field's own `prop(YES)` declaration
    /// decides ([`RecordInstance::field_posts_property`]). (The `Q:form`
    /// -> `display.form` mapping is exempt: it reads an immutable
    /// load-time info tag, not a runtime field.)
    metadata_cache: MetadataCell,
    /// `DBE_ALARM` when the last epilogue's `recGblResetAlarms` posted the
    /// alarm fields (its return value, `recGbl.c:209-212`), else empty. Set
    /// only by the alarm-moving monitor publication and taken by the same
    /// cycle's [`crate::server::record::ProcessAction::PutAfterEpilogue`], so
    /// no other cycle pays for it.
    alarm_reset: crate::server::recgbl::EventMask,
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

/// A field's answers from the type's fixed name lists: its
/// [`Record::fields_posted_with_value_mask`] gate, its [`AuxPostMask`] class
/// and its [`Record::prev_compared_inputs`] bit.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
struct FixedRoles {
    gate: Option<crate::server::record::ValuePostGate>,
    aux: u8,
    prev_bit: Option<u8>,
}

impl FixedRoles {
    /// By name: once per declared field when the plan is built, and on every
    /// lookup of a field no table declares.
    fn by_name(
        name: &str,
        value_masked: &[(&'static str, crate::server::record::ValuePostGate)],
        aux: &AuxPostMask,
        prev_compared: &[&'static str],
    ) -> Self {
        Self {
            gate: value_masked
                .iter()
                .find(|(field, _)| name_bytes_eq(field, name))
                .map(|(_, gate)| *gate),
            aux: aux
                .lists()
                .iter()
                .position(|list| names_contain(list, name))
                .map_or(0, |i| i as u8 + 1),
            prev_bit: prev_compared
                .iter()
                .position(|field| name_bytes_eq(field, name))
                .map(|bit| bit as u8),
        }
    }
}

/// What a record TYPE answers to the type-static questions C `monitor()`
/// asks about it — the deadband field and its mask narrowings, the value
/// gate, and whether the record posts its value at all. Every answer is a
/// property of the type, so it is read once at construction rather than
/// through the `dyn Record` vtable on every cycle. The per-cycle questions
/// (`monitor_value_changed`, `process_posted_fields`, the `take_*` one-shot
/// masks) stay dynamic.
#[derive(Clone)]
pub(crate) struct MonitorPlan {
    /// [`Record::process_posts_value_monitor`].
    pub(crate) posts_value_monitor: bool,
    /// [`Record::alarm_alone_posts_value`].
    pub(crate) alarm_alone_posts_value: bool,
    /// [`Record::uses_monitor_deadband`] and [`Record::has_array_monitor_post`].
    pub(crate) value_gate: crate::server::record::MonitorValueGate,
    /// [`Record::monitor_deadband_field`].
    pub(crate) deadband_field: &'static str,
    /// `deadband_field`'s declaration, so the post reads a non-VAL deadband
    /// field without a name scan.
    pub(crate) deadband_desc: Option<&'static FieldDesc>,
    /// `deadband_field`'s declaration when it is spelled as declared, for its
    /// [`PostName`].
    deadband_declared: Option<&'static FieldDesc>,
    /// `deadband_declared`'s [`FieldIx`].
    deadband_ix: FieldIx,
    /// `deadband_field` is `VAL`, read through [`Record::val`].
    deadband_is_val: bool,
    /// The deadband field's log is a reference ([`posts_by_ref`]): its post
    /// carries no value and never reads the field.
    pub(crate) deadband_ref: bool,
    /// The deadband field is in [`Record::value_only_change_fields`] or
    /// [`Record::fields_posted_with_monitor_mask`]: C posts it without
    /// `DBE_LOG`.
    pub(crate) log_suppressed: bool,
    /// [`Record::fields_posted_with_value_mask`].
    pub(crate) value_masked: &'static [(&'static str, crate::server::record::ValuePostGate)],
    /// Bit `i`: `value_masked[i]`'s log is a reference ([`posts_by_ref`]).
    pub(crate) value_masked_ref: u64,
    /// Bit `i`: `value_masked[i]` keeps no published copy
    /// ([`Self::stores_nothing`]), so its post with no subscriber is nothing.
    value_masked_silent: u64,
    /// `value_masked[i]`'s [`FieldDesc::name`], for its post's [`PostName`];
    /// `None` past the fourth or for a field spelled other than declared.
    value_masked_declared: [Option<&'static FieldDesc>; 4],
    /// `value_masked` holds a field its block posts on a cycle VAL's own mask
    /// is shut, so [`Record::take_secondary_value_changes`] runs every cycle;
    /// otherwise only on a cycle VAL posts.
    ///
    /// [`ValuePostGate::OnChangeForced`]: crate::server::record::ValuePostGate::OnChangeForced
    pub(crate) forced_secondary: bool,
    /// The change-detected aux fields' mask resolver.
    pub(crate) aux_post: AuxPostMask,
    /// [`Record::prev_compared_inputs`]' names.
    prev_compared: &'static [&'static str],
    /// The [`FixedRoles`] of every declared field a fixed list names, so a
    /// declared field's are found by index.
    fixed: Box<[(FieldIx, FixedRoles)]>,
    /// [`Record::fields_posted_only_when_marked`]: the fields whose post
    /// decisions the type owns, so the framework keeps no published copy of
    /// them ([`RecordInstance::record_value_post`]).
    pub(crate) claimed: &'static [&'static str],
    /// `claimed`'s declared fields.
    claimed_bits: FieldBits,
    /// [`Record::answers_own_posts`].
    answers_own_posts: bool,
    /// [`Record::process_posted_fields`] answers `Some`, which is fixed per
    /// type, so a type with an open set is not asked every cycle.
    posts_closed: bool,
    /// The declared fields [`walk_skip_term`] answers for.
    walk_skip_bits: FieldBits,
    /// The `HASH` field's declaration, for the array types' hash post.
    hash_desc: Option<&'static FieldDesc>,
    /// The `VAL` field's declaration, C's `indvalFlddes`.
    val_desc: Option<&'static FieldDesc>,
    /// [`FieldDeclaration::field_list`], which [`FieldIx`] indexes past `dbCommon`.
    fields: &'static [FieldDesc],
    /// Each declared field's [`FieldRead`], by [`FieldIx`].
    reads: Box<[FieldRead]>,
    /// [`Record::cycle_hooks`].
    pub(crate) hooks: crate::server::record::CycleHooks,
    /// Whether the type is `sub` or `aSub` — a property of the type, so not
    /// matched by name every cycle.
    subroutine_kind: Option<SubroutineKind>,
}

/// What a cycle's monitor state half asks before it knows anyone listens: the
/// [`MonitorPlan`] answers it reads, whether a monitor is attached and whether
/// a field was ever published. One word on the tail line the cycle already
/// holds, so a record nobody watches reads none of the plan's lines, the
/// subscriber list's or `last_posted`'s — C's `monitor()` reads its record
/// and nothing else.
///
/// The plan bits are copied once from the plan, which never changes. The two
/// others have one writer each: [`RecordInstance::set_subscribers_watched`],
/// after every change to the list, and [`RecordInstance::record_value_post_as`],
/// the only insert into `last_posted`, which never shrinks.
#[derive(Clone, Copy)]
struct MonitorHot {
    bits: u16,
    /// [`MonitorPlan::deadband_member`]'s index, read under [`Self::DEADBAND_DECLARED`].
    deadband_ix: FieldIx,
    /// [`MonitorPlan::value_gate`], kept whole: it is handed on as two bools.
    gate: crate::server::record::MonitorValueGate,
}

impl MonitorHot {
    /// [`MonitorPlan::posts_value_monitor`].
    const POSTS_VALUE: u16 = 1 << 0;
    /// [`MonitorPlan::log_suppressed`].
    const LOG_SUPPRESSED: u16 = 1 << 3;
    /// [`MonitorPlan::alarm_alone_posts_value`].
    const ALARM_ALONE: u16 = 1 << 4;
    /// [`MonitorPlan::hooks`] has [`CycleHooks::CYCLE_POSTED`](crate::server::record::CycleHooks::CYCLE_POSTED).
    const CYCLE_POSTED: u16 = 1 << 5;
    /// [`MonitorPlan::forced_secondary`].
    const FORCED_SECONDARY: u16 = 1 << 6;
    /// [`MonitorPlan::value_masked`] is not empty.
    const VALUE_MASKED: u16 = 1 << 7;
    /// [`MonitorPlan::answers_own_posts`].
    const ANSWERS_OWN: u16 = 1 << 8;
    /// [`MonitorPlan::deadband_member`] is `Some`.
    const DEADBAND_DECLARED: u16 = 1 << 9;
    /// A monitor is attached.
    const WATCHED: u16 = 1 << 10;
    /// `last_posted` holds an entry.
    const PUBLISHED: u16 = 1 << 11;
    /// Any set, the cycle may owe a mark TAKE or the walk a field to reach.
    const WALK_OWED: u16 = Self::CYCLE_POSTED | Self::WATCHED | Self::PUBLISHED;

    fn of(plan: &MonitorPlan) -> Self {
        let bit = |on: bool, bit: u16| if on { bit } else { 0 };
        let deadband = plan.deadband_member();
        Self {
            bits: bit(plan.posts_value_monitor, Self::POSTS_VALUE)
                | bit(plan.log_suppressed, Self::LOG_SUPPRESSED)
                | bit(plan.alarm_alone_posts_value, Self::ALARM_ALONE)
                | bit(
                    plan.hooks
                        .has(crate::server::record::CycleHooks::CYCLE_POSTED),
                    Self::CYCLE_POSTED,
                )
                | bit(plan.forced_secondary, Self::FORCED_SECONDARY)
                | bit(!plan.value_masked.is_empty(), Self::VALUE_MASKED)
                | bit(plan.answers_own_posts, Self::ANSWERS_OWN)
                | bit(deadband.is_some(), Self::DEADBAND_DECLARED),
            deadband_ix: deadband.unwrap_or(FieldIx::UNDECLARED),
            gate: plan.value_gate,
        }
    }

    #[inline(always)]
    fn has(self, bit: u16) -> bool {
        self.bits & bit != 0
    }

    #[inline(always)]
    fn set(&mut self, bit: u16, on: bool) {
        if on {
            self.bits |= bit;
        } else {
            self.bits &= !bit;
        }
    }

    #[inline(always)]
    fn value_gate(self) -> crate::server::record::MonitorValueGate {
        self.gate
    }

    #[inline(always)]
    fn deadband_member(self) -> Option<FieldIx> {
        self.has(Self::DEADBAND_DECLARED)
            .then_some(self.deadband_ix)
    }
}

impl MonitorPlan {
    /// The key a declared field posted under its declared name is filed by.
    fn key_of(&self, desc: &'static FieldDesc) -> PostKey<'static> {
        match FieldIx::in_table(self.fields, Some(desc)) {
            FieldIx::UNDECLARED => PostKey::Named(desc.name),
            ix => PostKey::Ix(ix),
        }
    }

    /// How the field declared `desc` is read.
    #[inline]
    fn read_of(&self, desc: &'static FieldDesc) -> FieldRead {
        // The type's own table first: a read names an own field far more
        // often than a `dbCommon` one.
        let common = crate::server::record::dbd_generated::DB_COMMON_FIELDS;
        let size = std::mem::size_of::<FieldDesc>();
        let at = desc as *const FieldDesc as usize;
        let own = at.wrapping_sub(self.fields.as_ptr() as usize);
        let ix = if own < std::mem::size_of_val(self.fields) {
            common.len() + own / size
        } else {
            let off = at.wrapping_sub(common.as_ptr() as usize);
            if off >= std::mem::size_of_val(common) {
                return if desc.unreadable() {
                    FieldRead::Unreadable
                } else {
                    FieldRead::Named
                };
            }
            off / size
        };
        self.reads[ix]
    }

    /// The declaration at `ix`.
    fn desc_at(&self, ix: FieldIx) -> Option<&'static FieldDesc> {
        crate::server::record::field_at(self.fields, ix)
    }

    /// The deadband field's index when it is spelled as declared.
    fn deadband_member(&self) -> Option<FieldIx> {
        self.deadband_declared.map(|_| self.deadband_ix)
    }

    /// The [`FixedRoles`] of the field filed under `key`.
    fn fixed_roles(&self, key: PostKey<'_>) -> FixedRoles {
        match key {
            PostKey::Ix(ix) => self
                .fixed
                .iter()
                .find(|(at, _)| *at == ix)
                .map_or_else(FixedRoles::default, |(_, roles)| *roles),
            PostKey::Named(name) => {
                FixedRoles::by_name(name, self.value_masked, &self.aux_post, self.prev_compared)
            }
        }
    }

    /// Whether a post filed under `key` stores nothing: the deadband field,
    /// whose published value is MLST/ALST, or a field the type claims.
    fn stores_nothing(&self, key: PostKey<'_>) -> bool {
        match key {
            PostKey::Ix(ix) => ix == self.deadband_ix || self.claimed_bits.has(ix),
            PostKey::Named(name) => {
                name_bytes_eq(name, self.deadband_field) || names_contain(self.claimed, name)
            }
        }
    }

    pub(crate) fn of(record: &dyn Record) -> Self {
        let deadband_field = record.monitor_deadband_field();
        let deadband_desc = field_desc_of(record, deadband_field);
        let deadband_declared = declared_desc(deadband_desc, deadband_field);
        let value_masked = record.fields_posted_with_value_mask();
        let claimed = record.fields_posted_only_when_marked();
        let event_posted = record.event_posted_fields();
        let aux_post = AuxPostMask::of(record);
        let prev_compared = record.prev_compared_inputs().0;
        let mut fixed: Vec<(FieldIx, FixedRoles)> = Vec::new();
        let named = value_masked.iter().map(|(name, _)| *name);
        let named = named.chain(aux_post.lists().into_iter().flatten().copied());
        for name in named.chain(prev_compared.iter().copied()) {
            let ix = FieldIx::of(record, declared_desc(field_desc_of(record, name), name));
            if ix != FieldIx::UNDECLARED && fixed.iter().all(|(at, _)| *at != ix) {
                let roles = FixedRoles::by_name(name, value_masked, &aux_post, prev_compared);
                fixed.push((ix, roles));
            }
        }
        let mut plan = Self {
            posts_value_monitor: record.process_posts_value_monitor(),
            alarm_alone_posts_value: record.alarm_alone_posts_value(),
            value_gate: crate::server::record::MonitorValueGate {
                uses_deadband: record.uses_monitor_deadband(),
                array_monitor_post: record.has_array_monitor_post(),
            },
            deadband_field,
            deadband_desc,
            deadband_declared,
            deadband_ix: FieldIx::of(record, deadband_declared),
            deadband_is_val: deadband_field == "VAL",
            deadband_ref: posts_by_ref(deadband_desc, || {
                if deadband_field == "VAL" {
                    record.val()
                } else {
                    record.get_field(deadband_field)
                }
            }),
            log_suppressed: record.value_only_change_fields().contains(&deadband_field)
                || record
                    .fields_posted_with_monitor_mask()
                    .contains(&deadband_field),
            value_masked,
            value_masked_silent: 0,
            value_masked_ref: value_masked
                .iter()
                .enumerate()
                .filter(|(_, (name, _))| {
                    posts_by_ref(field_desc_of(record, name), || record.get_field(name))
                })
                .fold(0, |bits, (i, _)| bits | 1 << i),
            value_masked_declared: std::array::from_fn(|i| {
                value_masked
                    .get(i)
                    .and_then(|(name, _)| declared_desc(field_desc_of(record, name), name))
            }),
            forced_secondary: record
                .fields_posted_with_value_mask()
                .iter()
                .any(|(_, gate)| {
                    matches!(
                        gate,
                        crate::server::record::ValuePostGate::OnChangeForced
                            | crate::server::record::ValuePostGate::Guarded
                    )
                }),
            aux_post,
            prev_compared,
            fixed: fixed.into(),
            claimed,
            claimed_bits: FieldBits::of(record, |desc| claimed.contains(&desc.name)),
            answers_own_posts: record.answers_own_posts(),
            posts_closed: record.process_posted_fields().is_some(),
            walk_skip_bits: FieldBits::of(record, |desc| {
                walk_skip_term(desc.name, deadband_field, event_posted)
            }),
            hash_desc: declared_desc(field_desc_of(record, "HASH"), "HASH"),
            val_desc: declared_desc(field_desc_of(record, "VAL"), "VAL"),
            fields: record.field_list(),
            reads: FieldRead::table(record),
            hooks: record.cycle_hooks(),
            subroutine_kind: SubroutineKind::of(record.record_type()),
        };
        plan.value_masked_silent = value_masked
            .iter()
            .enumerate()
            .filter(|(_, (name, _))| {
                plan.stores_nothing(match declared_desc(field_desc_of(record, name), name) {
                    Some(desc) => plan.key_of(desc),
                    None => PostKey::Named(name),
                })
            })
            .fold(0, |bits, (i, _)| bits | 1 << i);
        debug_assert!(
            !plan.answers_own_posts || plan.own_posts_hold(record),
            "{} answers its own posts but its post sets say otherwise",
            record.record_type()
        );
        plan
    }

    /// `take`'s fired bits are ones their [`ValuePostGate`] lets in.
    ///
    /// [`ValuePostGate`]: crate::server::record::ValuePostGate
    fn secondary_fits(
        &self,
        take: crate::server::record::SecondaryTake,
        value_mask: EventMask,
    ) -> bool {
        use crate::server::record::ValuePostGate;
        let guard = value_mask | take.widen();
        (0..u64::BITS as usize)
            .filter(|i| take.fired() & (1 << i) != 0)
            .all(|i| match self.value_masked.get(i) {
                Some((_, ValuePostGate::OnChange)) => !value_mask.is_empty(),
                Some((_, ValuePostGate::OnChangeForced | ValuePostGate::Guarded)) => {
                    !guard.is_empty()
                }
                _ => false,
            })
    }

    /// [`Record::answers_own_posts`]' claim, held against the record's sets.
    fn own_posts_hold(&self, record: &dyn Record) -> bool {
        record.process_posted_fields().is_some_and(|set| {
            set.iter()
                .all(|&ix| self.walk_skip_bits.has(ix) || self.claimed_bits.has(ix))
        }) && self
            .value_masked
            .iter()
            .all(|(_, gate)| !matches!(gate, crate::server::record::ValuePostGate::WithValue))
            && record.force_posted_fields().is_empty()
            && record.alarm_cycle_monitored_fields().is_empty()
            && record.log_swept_fields().is_empty()
    }
}

/// [`RecordInstance::declared_projection`] from the field's declared type and
/// long-string flag.
fn declared_projection_of(
    declared: Option<DbFieldType>,
    value: &EpicsValue,
    long_string: impl FnOnce() -> bool,
) -> Option<DbFieldType> {
    match declared? {
        DbFieldType::String
            if matches!(value, EpicsValue::CharArray(_) | EpicsValue::UCharArray(_))
                && long_string() =>
        {
            None
        }
        declared
            if value.db_field_type() == declared
                && !matches!(value, EpicsValue::EnumWithChoices { .. }) =>
        {
            None
        }
        declared => Some(declared),
    }
}

/// `desc`'s own name when `field` is its declared spelling.
fn declared_desc(desc: Option<&'static FieldDesc>, field: &str) -> Option<&'static FieldDesc> {
    desc.filter(|d| d.name == field)
}

/// The [`PostName`] of `field`, whose declaration is `declared` when it is
/// spelled as declared.
fn post_name(declared: Option<&'static FieldDesc>, field: &'static str) -> PostName {
    match declared {
        Some(desc) => PostName::Declared(desc),
        None => PostName::Named(Cow::Borrowed(field)),
    }
}

/// Whether a post of the field `desc` declares carries no value: C's
/// `useValque` fails for it (`dbEvent.c:493-500`), so its log is a reference
/// read at delivery. A type with no field table declares a field by the value
/// it stores (`stored`); one it cannot read posts nothing.
fn posts_by_ref(
    desc: Option<&'static FieldDesc>,
    stored: impl FnOnce() -> Option<EpicsValue>,
) -> bool {
    use crate::server::event_queue::LogKind;
    match desc {
        Some(_) => LogKind::of_field(desc, || None) == LogKind::Ref,
        None => stored().is_some_and(|v| LogKind::of_value(&v) == LogKind::Ref),
    }
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
    field_desc_in(record.field_list(), field)
}

/// C's `getEnumString` dispatch for `field` of `record`; `device` answers
/// `DTYP`'s choices. See [`RecordInstance::enum_string_form_for`].
fn enum_string_form_with(
    record: &dyn Record,
    field: &str,
    device: impl FnOnce() -> Option<Vec<PvString>>,
) -> Option<EnumStringForm> {
    let desc = field_desc_of(record, field);
    if is_device(desc) {
        // `None` propagates C's `goto nostrs` (`dbAccess.c:178`): a record
        // type with no `device()` declaration supplies no choice list, so
        // the leaf is omitted rather than marked empty.
        return device().map(EnumStringForm::device);
    }
    if let Some(choices) = menu_choices_with(record, field, desc) {
        return Some(EnumStringForm::menu(
            choices.iter().map(|c| PvString::from(*c)),
        ));
    }
    if field.eq_ignore_ascii_case("VAL") {
        return record.enum_slots().map(EnumSlots::form);
    }
    None
}

/// The per-field half of C's `dbGet` options — the rset slots, keyed by
/// field — over the record-level metadata cache. Everything it reads is the
/// record type's own code and declarations, so a record inside its own
/// processing serves it from `&dyn Record` ([`own_field_snapshot`]) as C's
/// lock-free `dbGet` does.
pub(crate) trait FieldMetaSource {
    fn rec(&self) -> &dyn Record;
    /// A field of the record by name; the `FTVL` test reads it.
    fn read_field(&self, field: &str) -> Option<EpicsValue>;
    /// [`RecordInstance::enum_string_form_for`].
    fn enum_string_form(&self, field: &str) -> Option<EnumStringForm>;

    /// `meta`, the record's cached VAL metadata, finished for `field`.
    fn finish_metadata(
        &self,
        field: &str,
        meta: MetadataSnapshot,
        backing: LinkBacking<'_>,
        snap: &mut super::super::snapshot::Snapshot,
    ) {
        let explicit_alarm = meta.alarm;
        snap.display = meta.display;
        snap.control = meta.control;
        snap.enums = meta.enums;

        // The cache above is the record's VAL metadata. C routes PER FIELD, so
        // a non-VAL-class field does NOT get VAL's limits — see
        // [`Self::route_field_metadata`], which owns that decision.
        self.route_field_metadata(field, backing, explicit_alarm, snap);

        // Per-field RSET metadata (C get_units/get_precision/
        // get_graphic_double/get_control_double/get_alarm_double key on
        // dbGetFieldIndex) patches the record-level cache for this field.
        self.apply_field_metadata_override(field, snap);

        // DBF_MENU field (a shared menu such as `SCAN`/`OMSL`/`HHSV`/... or
        // a record-specific menu such as `sel.SELM`): carry the menu index
        // as DBR_ENUM and attach its `menu()` choice labels. See
        // `attach_menu_enum`. This overrides any record VAL enum table
        // copied from the metadata cache above, because a menu field
        // carries its own menu's choices, not the record's VAL state
        // strings.
        self.attach_menu_enum(field, snap);

        // The metadata VALUES and the mask that says which of them this
        // channel actually supplies are assigned by the same owner, from the
        // settled value, so they cannot disagree.
        self.assign_property_support(field, snap);
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
    /// owner here: [`RecordInstance::control_explicit_field`],
    /// [`RecordInstance::graphic_explicit_field`] and [`RecordInstance::alarm_explicit_field`].
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
        explicit_alarm: (f64, f64, f64, f64),
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
        let slots = self.rec().property_support();
        let rtype = self.rec().record_type();
        let f: &str = &upper_field(field);

        // C's `get_linkNumber` question, asked ONCE per snapshot: which of this
        // record's own link fields, if any, supplies this field's metadata.
        // Four of the six slots consult it — `aSubRecord.c:306-404` is the
        // complete specimen — and control does not, because
        // `dbGetControlLimits` has no caller anywhere in base.
        let link_backed = self.rec().link_backed_metadata_field(f);
        // Resolved for THIS build and handed in — see [`LinkBacking`]. Reading
        // a value the record had stored is what made a `caput SRC.EGU` invisible
        // to a passive source's clients until the source next processed.
        let link_meta = link_backed.as_ref().and_then(|lf| backing.metadata(lf));

        // C `get_units`'s "no case" arm — the field the rset tests for and
        // declines to write, leaving `dbAccess.c:378`'s zeroed buffer. The
        // record-level cache holds `EGU` for every type that supplies the
        // slot, so this is the step that takes it back off the fields C never
        // gives it to. See [`RecordInstance::units_from_egu`].
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
            } else if !self.units_from_egu(rtype, f) {
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
            // [`RecordInstance::precision_explicit_field`] — instead of thirty
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
            ) && !RecordInstance::precision_explicit_field(
                rtype,
                f,
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
        if slots.control_double && !RecordInstance::control_explicit_field(rtype, field) {
            let (upper, lower) =
                match super::record_trait::control_default_arm(self.rec().record_type()) {
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
        if slots.graphic_double && !RecordInstance::graphic_explicit_field(rtype, field) {
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
            let (hihi, high, low, lolo) = if RecordInstance::alarm_explicit_field(rtype, field) {
                explicit_alarm
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
    /// Every served `Snapshot`, a monitor update's delivery read included,
    /// runs this one owner, so
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
        let Some(ov) = self.rec().field_metadata_override(field) else {
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

    /// Attach a `DBF_MENU` field's `menu()` choice labels to a built snapshot,
    /// so the CA/PVA enum encoders present `"NO CONVERSION"` rather than `0`.
    ///
    /// The VALUE half of the `DBF_MENU` -> `DBR_ENUM` mapping is not here: the
    /// `.dbd` declares a menu field `DBF_MENU`, the generator types that
    /// `DbFieldType::Enum` (`mapDBFToDBR`), and
    /// [`RecordInstance::project_to_declared_type`] — which every delivery path runs —
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
        let Some(form) = self.enum_string_form(field) else {
            return;
        };
        snap.enums = Some(super::super::snapshot::EnumInfo::with_string_form(
            form.slots.clone(),
            form,
        ));
    }

    /// Stamp a built snapshot with the property mask THIS channel supplies —
    /// [`Record::property_support`] narrowed to the addressed field by C's
    /// second gate ([`PropertySupport::narrowed_to_field`]). Called by both
    /// snapshot builders once the value has settled (after
    /// [`RecordInstance::attach_menu_enum`] promoted a `DBF_MENU` field to its
    /// `DBR_ENUM` form), so the mask is read off the same value the client
    /// receives and no consumer has to re-derive either gate.
    fn assign_property_support(&self, field: &str, snap: &mut super::super::snapshot::Snapshot) {
        snap.properties = self.rec().property_support().narrowed_to_field(
            snap.value.db_field_type(),
            menu_choices_of(self.rec(), field).is_some(),
        );
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
        let f: &str = &upper_field(field);
        // The link arm is NOT here: `route_field_metadata` asks
        // [`Record::link_backed_metadata_field`] first and only falls through
        // to this EGU question for a field no link backs. This function
        // answers C's `else strncpy(units, prec->egu, ...)` branch alone.
        let own = |t: T| self.static_field_type(f) == Some(t);
        match rtype {
            "ai" => own(T::Double) && !matches!(f, "ASLO" | "AOFF" | "SMOO"),
            "ao" => own(T::Double) && !matches!(f, "ASLO" | "AOFF"),
            "calc" | "calcout" | "sub" | "sel" | "dfanout" => own(T::Double),
            "longin" | "longout" => own(T::Long),
            "int64in" | "int64out" => own(T::Int64),
            "compress" => own(T::Double) || f == "VAL",
            "waveform" | "aai" | "aao" | "subArray" => {
                matches!(f, "HOPR" | "LOPR") || (f == "VAL" && !self.ftvl_is_string_or_enum())
            }
            "histogram" | "seq" | "bo" | "table" | "aSub" => false,
            _ => true,
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
        let desc = field_desc_of(self.rec(), field)?;
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
        let desc = field_desc_of(self.rec(), field)?;
        crate::server::recgbl::rec_gbl_get_graphic_double(desc.declared_dbf)
    }

    /// `FTVL` names `DBF_STRING` or `DBF_ENUM` — the two element types for
    /// which the array rsets break out of the `VAL` case before copying `EGU`.
    /// `menuFtype` is declared in `DBF_` code order, so `0` is STRING and `11`
    /// is ENUM.
    fn ftvl_is_string_or_enum(&self) -> bool {
        matches!(
            self.read_field("FTVL").and_then(|v| v.to_f64()),
            Some(x) if x == 0.0 || x == 11.0
        )
    }
}

impl FieldMetaSource for RecordInstance {
    fn rec(&self) -> &dyn Record {
        self.record.as_ref()
    }
    fn read_field(&self, field: &str) -> Option<EpicsValue> {
        self.resolve_field(field)
    }
    fn enum_string_form(&self, field: &str) -> Option<EnumStringForm> {
        self.enum_string_form_for(field)
    }
}

/// A record read from inside its own processing: its type's code and
/// nothing behind its lock set.
struct OwnRecord<'a>(&'a dyn Record);

impl FieldMetaSource for OwnRecord<'_> {
    fn rec(&self) -> &dyn Record {
        self.0
    }
    fn read_field(&self, field: &str) -> Option<EpicsValue> {
        self.0.get_field(field)
    }
    fn enum_string_form(&self, field: &str) -> Option<EnumStringForm> {
        // `DTYP` lives in dbCommon, behind the lock set.
        enum_string_form_with(self.0, field, || None)
    }
}

/// C `dbGet` of `field` (canonical) of `cell`, from inside that record's own
/// processing: the value and the field's metadata from `record`, the record
/// itself, and the metadata cache, which needs no lock. `None` for a field
/// the record type does not serve itself (dbCommon's) or whose metadata cache
/// init has not built. A link-backed field gets its C seeds: resolving its
/// link would read through the lock set this record holds.
pub(crate) fn own_field_snapshot(
    cell: &RecordCell,
    record: &dyn Record,
    field: &str,
) -> Option<super::super::snapshot::Snapshot> {
    let meta = cell.metadata.load_full()?;
    let value = record.get_field(field)?;
    let value = match declared_projection_of(declared_field_type_of(record, field), &value, || {
        record.field_is_long_string(field)
    }) {
        Some(declared) => value.into_type(declared),
        None => value,
    };
    let mut snap =
        super::super::snapshot::Snapshot::new(value, 0, 0, std::time::SystemTime::UNIX_EPOCH);
    OwnRecord(record).finish_metadata(field, (*meta).clone(), LinkBacking::none(), &mut snap);
    Some(snap)
}

/// [`field_desc_of`] against a type's table — C `dbFindFieldPart`: the
/// record type's own `.dbd` table, then `dbCommon`.
pub(crate) fn field_desc_in(
    fields: &'static [FieldDesc],
    field: &str,
) -> Option<&'static FieldDesc> {
    let named = |t: &'static [FieldDesc]| t.iter().find(|f| f.name.eq_ignore_ascii_case(field));
    named(fields).or_else(|| named(super::dbd_generated::DB_COMMON_FIELDS))
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
    menu_choices_with(record, field, field_desc_of(record, field))
}

/// [`menu_choices_of`] for a caller that already looked up `desc`.
pub(crate) fn menu_choices_with<R: Record + ?Sized>(
    record: &R,
    field: &str,
    desc: Option<&'static FieldDesc>,
) -> Option<&'static [&'static str]> {
    // A `DBF_DEVICE` field's choices are the record type's DEVICE menu
    // (C `dbDeviceMenu`, built from the `device()` declarations), which is
    // per-record-type and so cannot live in the shared `dbCommon` FieldDesc.
    if is_device(desc) {
        return super::dbd_generated::device_menu(record.record_type());
    }
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

/// `desc` is declared `DBF_DEVICE` (dbCommon's `DTYP`).
pub(crate) fn is_device(desc: Option<&FieldDesc>) -> bool {
    desc.is_some_and(|f| f.declared_dbf == crate::types::DbfCode::Device)
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

/// The `DBF_*` type a link to `field` addresses — C `dbNameToAddr`'s
/// `field_type` once `cvt_dbaddr` has run: the declaration, or, for a
/// runtime-typed field, the variant the record holds.
pub(crate) fn addressed_field_type_of<R: Record + ?Sized>(
    record: &R,
    field: &str,
) -> Option<DbFieldType> {
    declared_field_type_of(record, field)
        .or_else(|| record.get_field(field).map(|v| v.db_field_type()))
}

/// The source record's part of a `DBR_STRING` link read — C's
/// `[field_type][DBR_STRING]` conversion state — captured by
/// [`RecordInstance::source_render_of`] under the read's hold and applied
/// after it is released.
#[derive(Debug)]
pub(crate) struct SourceRender {
    long_string: bool,
    /// `getDoubleString`'s precision; a float value only.
    precision: Option<u16>,
    /// The state label of an index value whose field has a string source.
    label: Option<PvString>,
}

impl SourceRender {
    /// The render of a record seen only through its [`Record`] face: no
    /// display precision or choice table to apply.
    pub(crate) fn of_record(rec: &dyn Record, field: &str, value: &EpicsValue) -> Self {
        use crate::server::record::record_trait::FieldDeclaration;
        Self {
            long_string: value.is_array() && rec.field_is_long_string(field),
            precision: None,
            label: None,
        }
    }

    /// The value as C `dbGetLink(plink, DBR_STRING, ...)` delivers it: one
    /// element, so an array source converts its element 0 (none, at NORD 0,
    /// is a failed read).
    pub(crate) fn dbr_string(self, value: EpicsValue) -> Option<PvString> {
        if self.long_string
            && let EpicsValue::CharArray(b) | EpicsValue::UCharArray(b) = &value
            && !b.is_empty()
        {
            return Some(PvString::from_bytes(
                crate::server::database::char_view::long_string_field_bytes(b),
            ));
        }
        let value = if value.is_array() && !self.long_string {
            value.first_element()?
        } else {
            value
        };
        if let Some(prec) = self.precision
            && let Some(EpicsValue::String(s)) =
                crate::types::codec::dbr_string_at_precision(&value, prec)
        {
            return Some(s);
        }
        if matches!(value, EpicsValue::Enum(_) | EpicsValue::Short(_))
            && let Some(label) = self.label
        {
            return Some(label);
        }
        value_as_dbr_string(&value)
    }

    /// [`Self::dbr_string`] for every element of an array value, as `dbGet`
    /// converts `nRequest > 1` elements into a `DBR_STRING` buffer.
    /// A long-string field is one element, so `n_request` cuts only a real
    /// array.
    pub(crate) fn dbr_strings(self, mut value: EpicsValue, n_request: usize) -> Option<EpicsValue> {
        if !value.is_array() || self.long_string {
            return self.dbr_string(value).map(EpicsValue::String);
        }
        value.truncate(n_request);
        match self
            .precision
            .and_then(|prec| crate::types::codec::dbr_string_at_precision(&value, prec))
        {
            Some(rendered) => Some(rendered),
            None => value.get_convert(DbFieldType::String).ok(),
        }
    }
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
        other => match other.convert_to(DbFieldType::String) {
            EpicsValue::String(s) => Some(s),
            _ => None,
        },
    }
}

/// The previous-value trackers a C `monitor()` compares against and never
/// posts: `oraw`/`orbv` (the last RVAL/RBV) on the raw-value records, and
/// `mlst`/`alst`/`lalm` (the last VAL the deadbands compare to) on every type,
/// mbboDirect's `obit` (the last bits posted), the output records' `pval`
/// (the last VAL the OOPT test or DOL fetch compares to), and `afvl`, the
/// alarm filter's accumulator `checkAlarms` stores (aiRecord.c:401).
/// A `caput` to one still posts, through the put path.
const MONITOR_TRACKER_FIELDS: [&str; 8] = [
    "ORAW", "ORBV", "MLST", "ALST", "LALM", "OBIT", "PVAL", "AFVL",
];

/// The fields `collect_subscriber_posts`'s walk discards before it
/// change-detects them, on every cycle of the record's life. Both the walk and
/// [`RecordInstance::walk_skips`] (which keeps these fields out of
/// `last_posted`) ask here, so the gate and the walk cannot disagree.
///
/// The deadband field is posted by the caller with its own C mask. No C
/// `monitor()` posts a `dbCommon` field: SEVR/STAT/AMSG/ACKS come from
/// recGbl.c:202-222 alone, and LCNT, UDF and the rest from nowhere outside
/// the dbAccess.c put paths. Nor does it post a [`MONITOR_TRACKER_FIELDS`]
/// entry. [`Record::event_posted_fields`] post from their own event path. A
/// `caput` to any of them still posts, through the put path.
fn walk_skip_term(field: &str, deadband_field: &str, event_posted: &[&str]) -> bool {
    field == deadband_field
        || MONITOR_TRACKER_FIELDS.contains(&field)
        || event_posted.contains(&field)
        || super::dbd_generated::DB_COMMON_FIELDS
            .iter()
            .any(|d| d.name == field)
}

impl RecordInstance {
    /// Name what the next cycle reads past the cell into `hints`: the OUT
    /// link's parse and `DbLink`, and the first two monitors' queue slots.
    #[inline(always)]
    pub(crate) fn publish_hints(&self, hints: &crate::server::database::CycleHints) {
        let parse = self.parsed_out.shared();
        hints.out[0].set_arc(parse);
        match &**parse {
            super::ParsedLink::Db(link) => hints.out[1].set_arc(link),
            _ => hints.out[1].clear(),
        }
        let mut subs = self.subscribers.iter();
        for pair in &hints.posts {
            match subs.next() {
                Some(s) => s.sub().sink.publish_hints(&pair[0], &pair[1]),
                None => pair.iter().for_each(|h| h.clear()),
            }
        }
    }

    pub fn new(name: impl Into<crate::types::RecordName>, record: impl Record) -> Self {
        Self::new_boxed(name, Box::new(record))
    }

    /// The raw text of one `dbCommon` link field, or `None` for any
    /// other field name.
    pub fn common_link_text(&self, field: &str) -> Option<&[u8]> {
        Some(match field {
            "INP" => self.common.text.inp.as_bytes(),
            "OUT" => self.common.text.out.as_bytes(),
            "TSEL" => self.common.text.tsel.as_bytes(),
            "SDIS" => self.common.text.sdis.as_bytes(),
            "FLNK" => self.common.text.flnk.as_bytes(),
            _ => return None,
        })
    }

    /// Install `link` as the stored link of `field` — its slot, else its
    /// `dbCommon` link cache, the order `Self::stored_link` reads —
    /// and return the parse it replaced, or hand `link` back for a field that
    /// is not one of the record's links. Either way nothing is allocated or
    /// freed here: the caller builds the `Arc` before the record's hold and
    /// drops what comes back after it, as C `dbPutFieldLink` parses ahead of
    /// `dbScanLock`. The only writer of the five caches (`LinkCache` has no
    /// other way in), so the plan stamp they feed is bumped here and cannot
    /// be skipped.
    pub fn install_link_cache(
        &mut self,
        field: &str,
        link: Arc<ParsedLink>,
    ) -> Result<Arc<ParsedLink>, Arc<ParsedLink>> {
        let mut link = link;
        if let Some(i) = self.link_slots.position(field) {
            self.invalidate_plan_for(field);
            self.link_slots.swap(i, &mut link);
            return Ok(link);
        }
        if self.swap_link_cache(field, &mut link) {
            Ok(link)
        } else {
            Err(link)
        }
    }

    /// [`Self::install_link_cache`] for an in-crate test that holds a bare
    /// parse; production installs only an `Arc` built before the hold.
    #[cfg(test)]
    pub(crate) fn set_link_cache(&mut self, field: &str, link: ParsedLink) -> bool {
        self.install_link_cache(field, Arc::new(link)).is_ok()
    }

    /// [`Self::install_link_cache`] handing the old parse back in `link`, so
    /// a caller under the hold can drop it after.
    fn swap_link_cache(&mut self, field: &str, link: &mut Arc<ParsedLink>) -> bool {
        match field {
            "INP" => self.parsed_inp.swap(link),
            "OUT" => self.parsed_out.swap(link),
            "TSEL" => self.parsed_tsel.swap(link),
            "SDIS" => self.parsed_sdis.swap(link),
            "FLNK" => self.parsed_flnk.swap(link),
            _ => return false,
        }
        if field == "TSEL" {
            let is_link = !self.parsed_tsel.is_constant();
            self.common.set_tsel_link(is_link);
        }
        self.invalidate_plan();
        true
    }

    /// The link fields whose target metadata this record's rset serves — the
    /// work list [`PvDatabase::resolve_link_backed_metadata`] resolves for a
    /// batch post, and the set `Self::link_backed_metadata_field_of` answers
    /// one field out of.
    ///
    /// [`PvDatabase::resolve_link_backed_metadata`]: crate::server::database::PvDatabase
    pub fn link_backed_metadata_links(&self) -> &[String] {
        &self.cold.link_backed_metadata_links
    }

    /// Where each [`Self::link_backed_metadata_links`] entry sits in the
    /// record's multi-input link list, in that same order. See the field.
    pub(crate) fn link_backed_metadata_input_slots(&self) -> &[Option<usize>] {
        &self.cold.link_backed_metadata_input_slot
    }

    /// The record type's [`Record::cycle_hooks`],
    /// as asked once at load.
    #[inline(always)]
    pub(crate) fn cycle_hooks(&self) -> crate::server::record::CycleHooks {
        self.monitor_plan.hooks
    }

    /// The record's context push, [`Record::process`]
    /// and the drain of what it posted
    /// ([`Record::take_process_posted_fields`]),
    /// as one step: a cycle path calls this, never `record.process()` alone,
    /// so a `PROCESS_POSTED` type cannot process without its posts being
    /// taken, before the cycle stamps the record or resets its alarm.
    ///
    /// The context push already tested the type's hooks; asking both hooks in
    /// that one test is what keeps a type with neither paying nothing for the
    /// drain.
    #[inline(always)]
    pub(crate) fn process_and_drain(
        &mut self,
    ) -> crate::error::CaResult<crate::server::record::ProcessOutcome> {
        use crate::server::record::CycleHooks;
        if self
            .monitor_plan
            .hooks
            .has(CycleHooks::CONTEXT.union(CycleHooks::PROCESS_POSTED))
        {
            return self.process_hooked();
        }
        self.process_record()
    }

    /// The one `Record::process` call, private to the two paths above.
    #[inline(always)]
    fn process_record(&mut self) -> crate::error::CaResult<crate::server::record::ProcessOutcome> {
        self.record.process()
    }

    /// [`Self::process_and_drain`] for a type with either hook. The drain
    /// runs on `Err` too: C posts from inside `process()` whatever it returns.
    #[inline(never)]
    fn process_hooked(&mut self) -> crate::error::CaResult<crate::server::record::ProcessOutcome> {
        self.push_process_context();
        let result = self.process_record();
        self.drain_process_posts();
        result
    }

    /// The drain of what a processing step posted: a type without
    /// `PROCESS_POSTED` pays this one bit test. Private: the callers are
    /// [`Self::process_hooked`] and [`Self::apply_invalid_output_value_and_drain`].
    #[inline(always)]
    fn drain_process_posts(&mut self) {
        if self
            .monitor_plan
            .hooks
            .has(crate::server::record::CycleHooks::PROCESS_POSTED)
        {
            self.emit_process_posts_marked();
        }
    }

    /// [`Record::apply_invalid_output_value`]
    /// and the drain of what it posted. A record's hook may re-run its own
    /// `process()` (ao's IVOA=Set_output_to_IVOV does, to reproduce C's
    /// `convert()`), so the hook is as much a processing step as
    /// [`Self::process_and_drain`] and is called only through here.
    #[inline(always)]
    pub(crate) fn apply_invalid_output_value_and_drain(
        &mut self,
        ivov: crate::types::EpicsValue,
    ) -> crate::error::CaResult<()> {
        let applied = self.record.apply_invalid_output_value(ivov);
        self.drain_process_posts();
        applied
    }

    /// The table names are upper-case, so the read skips `resolve_field`'s
    /// normalising copy.
    #[inline(never)]
    fn emit_process_posts_marked(&mut self) {
        use crate::server::recgbl::EventMask;
        use crate::server::record::CyclePostMask;
        let mut burst = PostBurst::new();
        for (ix, mask) in self.record.take_process_posted_fields() {
            let Some(desc) = self.monitor_plan.desc_at(ix) else {
                continue;
            };
            let mask = match mask {
                // No reset is in scope ahead of the cycle's commit.
                CyclePostMask::ResetAlarm => continue,
                CyclePostMask::Value | CyclePostMask::MonitorValue => EventMask::VALUE,
                CyclePostMask::MonitorLog => EventMask::LOG,
                CyclePostMask::ValueLog
                | CyclePostMask::MonitorValueLog
                | CyclePostMask::ResetValueLog
                | CyclePostMask::ResetValueLogOnAlarm => EventMask::VALUE | EventMask::LOG,
            };
            self.notify_declared(Some(&mut burst), desc, mask);
        }
    }

    /// Hand the record the framework-owned common state it reads during
    /// `process()` — only to a type whose [`Record::cycle_hooks`] says it
    /// takes it; C reads `dbCommon` in place and builds nothing.
    #[inline(always)]
    pub(crate) fn push_process_context(&mut self) {
        if self
            .monitor_plan
            .hooks
            .has(crate::server::record::CycleHooks::CONTEXT)
        {
            let ctx = self.common.process_context();
            self.record.set_process_context(&ctx);
        }
    }

    /// C `monitor()` and the `recGblResetAlarms` it opens with, for every
    /// path that ends a process cycle. The single owner of the cycle's
    /// monitor STATE — alarm commit, MLST/ALST, the previous-value store, the
    /// record's one-shot post masks — and of the posts it hands the
    /// publisher. The state half runs whether or not anyone subscribes;
    /// the per-field payload is built by [`Self::collect_subscriber_posts`]
    /// only for subscribed fields, decided under the guard the caller
    /// delivers with, so no post is decided on one lock and delivered on
    /// another.
    ///
    /// The posts land in the caller's `snapshot`, which is the size of a post:
    /// returned by value it was copied into the caller's slot on every cycle,
    /// the many that post nothing included. Returns the `recGblResetAlarms`
    /// posts with their per-field C masks.
    pub(crate) fn monitor_cycle(
        &mut self,
        snapshot: &mut ProcessSnapshot,
    ) -> crate::server::database::AlarmPosts {
        let alarm_result = if self
            .monitor_plan
            .hooks
            .has(crate::server::record::CycleHooks::MONITOR_RESET)
            && !self.record.monitor_resets_alarms()
        {
            crate::server::recgbl::AlarmResetResult::unchanged(&self.common)
        } else {
            crate::server::recgbl::rec_gbl_reset_alarms(&mut self.common)
        };
        // C `recGblResetAlarms` returns `val_mask = DBE_ALARM`
        // (recGbl.c:194/203/212) when the severity/status OR the alarm
        // message moved — every monitored-value post this cycle carries
        // DBE_ALARM so a `DBE_ALARM`-only subscriber sees the value at the
        // moment the alarm changed.
        let alarm_bits = if alarm_result.alarm_changed || alarm_result.amsg_changed {
            EventMask::ALARM
        } else {
            EventMask::NONE
        };
        let (include_val, include_archive, marked) = self.value_include_classes();
        // The deadband-tracked field posts with the classes that actually
        // fired: MDEL crossing → DBE_VALUE, ADEL crossing → DBE_LOG, alarm
        // movement → DBE_ALARM — and nothing else (C `monitor()` per-field
        // masks: motorRecord.cc:3476-3507 RBV, aiRecord.c VAL). A record
        // like motor deadbands its readback; its VAL then routes through
        // the change-detected loop in `collect_subscriber_posts`.
        let deadband_mask = self.deadband_post(snapshot, alarm_bits, include_val, include_archive);
        self.collect_subscriber_posts(snapshot, deadband_mask, alarm_bits, include_val, marked);
        // C waveform/aai/aao `monitor()` posts HASH with a literal
        // `DBE_VALUE` only on a content-hash change (waveformRecord.c:
        // 317-319), independent of the VAL post mask. `array_hash_changed`
        // was set by `check_deadband_ext` this cycle.
        if self.array_hash_changed {
            let hash = self.monitor_plan.hash_desc;
            if let Some(h) = hash.and_then(|desc| self.read_declared(desc)) {
                snapshot.push((
                    post_name(hash, "HASH"),
                    PostedValue::Val(h),
                    EventMask::VALUE,
                ));
            }
        }
        // NO `.UDF` post. C `monitor()` never posts UDF, and neither does
        // `recGblResetAlarms` (recGbl.c:202-222 posts SEVR/STAT/AMSG/ACKS
        // only). UDF reaches a `.UDF` subscriber only through the generic
        // put path (C `dbPut` posts the field it wrote, dbAccess.c:1411-1413).
        crate::server::database::alarm_field_posts(&self.common, &alarm_result)
    }

    /// The plan this cycle runs under, re-resolved when anything it was
    /// resolved from has changed since.
    #[inline(always)]
    pub(crate) fn cycle_plan(
        &mut self,
        plan: &ProcessPlan,
        is_continuation: bool,
    ) -> (CyclePlan, CycleFacts) {
        if self.cycle_plan.0 != self.plan_rev {
            self.replan(plan);
        }
        #[cfg(test)]
        if self.plan_forced_general {
            return (
                CyclePlan::General,
                CycleFacts {
                    not_simulating: false,
                    ..self.cycle_plan.2
                },
            );
        }
        let facts = self.cycle_plan.2;
        debug_assert!(
            !facts.not_simulating || self.record.simm_cells().is_some_and(|c| c.simm == 0),
            "SIMM changed without invalidate_plan"
        );
        let arm = match self.cycle_plan.1 {
            // What the stamp does not own: the device slot is set by init,
            // and a continuation re-enters with PACT held.
            CyclePlan::ConstSoftIn | CyclePlan::DbSoftIn
                if is_continuation || self.device.is_some() =>
            {
                CyclePlan::General
            }
            p => p,
        };
        debug_assert!(
            arm == CyclePlan::General || self.record.simm_cells().is_some_and(|c| c.simm == 0),
            "SIMM changed without invalidate_plan"
        );
        #[cfg(test)]
        {
            self.ran_arm = arm;
        }
        (arm, facts)
    }

    /// The arm the last cycle dispatched to, for the differential harness.
    #[cfg(test)]
    pub(crate) fn ran_arm(&self) -> CyclePlan {
        self.ran_arm
    }

    #[cold]
    #[inline(never)]
    fn replan(&mut self, plan: &ProcessPlan) {
        // SIMM is stamped: a put reaches it through `invalidate_plan_for`, and
        // the SIML read that also writes it runs only for a SIML that is not
        // a constant, which is neither plain nor `not_simulating`.
        let not_simulating = self.record.simm_cells().is_some_and(|c| c.simm == 0)
            && self.common.sscn.is_unset()
            && self.parsed_sdis.is_constant()
            && self.siml_has_const_lset();
        let facts = CycleFacts {
            not_simulating,
            device_posts: self.device.as_ref().is_some_and(|d| d.posts_from_device()),
        };
        let plain = plan.is_plain_input()
            && not_simulating
            && self.common.dtyp.is_soft()
            && plan.simm.is_some();
        let reads_double = || match self.parsed_inp.link() {
            ParsedLink::Db(db)
                if db.policy != crate::server::record::LinkProcessPolicy::ProcessPassive
                    && self.common.tse() != -2
                    && self.common.dtyp.soft()
                        != Some(crate::server::device_support::SoftDtyp::Raw) =>
            {
                matches!(
                    crate::server::record::InputLinkRequest::for_inp(
                        &*self.record,
                        self.common.dtyp.soft(),
                    ),
                    crate::server::record::InputLinkRequest::As(
                        crate::server::record::LinkReadAs::Double
                    )
                )
            }
            _ => false,
        };
        // An `Async Soft Channel` INP is notified, not read: the general
        // input stage owns that (`PvDatabase::async_soft_inp_stage`).
        let arm = if !plain || self.inp_is_pn_link() {
            CyclePlan::General
        } else if !self.dset_reads_inp() {
            CyclePlan::ConstSoftIn
        } else if reads_double() {
            CyclePlan::DbSoftIn
        } else {
            CyclePlan::General
        };
        self.cycle_plan = (self.plan_rev, arm, facts);
    }

    /// Whether the soft dset's `read_xxx` reaches INP this cycle: not for a
    /// constant (`dbConstGetValue` reads nothing), and not for an `Async`
    /// dset whose `add_record` refused the link.
    #[inline]
    pub(crate) fn dset_reads_inp(&self) -> bool {
        !self.parsed_inp.is_constant() && !self.common.dtyp.link_refused()
    }

    /// The single owner of [`CyclePlan`] staleness: whatever the plan was
    /// resolved from — a link text, DTYP, TSE, TSEL, SSCN, SIMM — changed.
    pub(crate) fn invalidate_plan(&mut self) {
        self.plan_rev = self.plan_rev.wrapping_add(1);
    }

    /// C `recGblGetTimeStampSimm(prec, prec->simm, siol)` for this record:
    /// `TSE == -2` under a non-NO `SIMM` stamps the current time when there is
    /// no SIOL to take a stamp from, which is every output record (`siol ==
    /// NULL`). An input's SIOL stamp is applied by the simulated-input tail.
    #[inline(always)]
    pub(crate) fn stamp_time_begun(
        &mut self,
        begun: crate::server::record::StampBegun,
        tsel: crate::server::database::TselStamp,
        output_before_siml: bool,
    ) {
        let record = &self.record;
        let before_siml = self.simm_before_siml_read.filter(|_| output_before_siml);
        let stored = tsel.stamp_begun(begun, &self.name, &mut self.common, || {
            !record.simulation_substitutes_input_stage()
                && before_siml
                    .or_else(|| record.simm_value())
                    .is_some_and(|simm| simm != 0)
        });
        if stored {
            self.hand_cycle_stamp();
        }
    }

    /// [`Record::time_stamped`] for a [`CycleHooks::STAMP_REQUEST`] pass whose
    /// stamp point just stored TIME, before the pass posts. A pass that did
    /// not ask stored nothing, so its stamp point does not get here.
    ///
    /// [`CycleHooks::STAMP_REQUEST`]: crate::server::record::CycleHooks::STAMP_REQUEST
    #[inline(always)]
    pub(crate) fn hand_cycle_stamp(&mut self) {
        if self.has_hook(crate::server::record::CycleHooks::STAMP_REQUEST) {
            self.record.time_stamped(self.common.time);
        }
    }

    /// [`Self::invalidate_plan`] when `field` is one the plan reads. Both
    /// field-put funnels — the common one and [`Self::put_declared_field`] —
    /// ask it, so the list of such fields has one home.
    pub(crate) fn invalidate_plan_for(&mut self, field: &str) {
        if matches!(
            field,
            "FLNK"
                | "INP"
                | "OUT"
                | "DTYP"
                | "TSE"
                | "TSEL"
                | "SSCN"
                | "SDIS"
                | "SIML"
                | "SIOL"
                | "SIMM"
        ) {
            self.invalidate_plan();
        }
        if field == "SIML" {
            self.simm_before_siml_read = None;
        }
    }

    /// [`Self::put_declared_field`] for the framework's own stores
    /// (`Record::put_field_internal`): device property posts, `post_fields`,
    /// `PutInLinkStage` stores.
    pub(crate) fn put_internal_field(&mut self, field: &str, value: EpicsValue) -> CaResult<()> {
        self.invalidate_plan_for(field);
        self.debug_assert_not_slotted(field, &value);
        let put = self.record.put_field_internal(field, value);
        self.refresh_dol_mode(field);
        put
    }

    /// The funnel for a put of a field the record TYPE declares (as opposed
    /// to `dbCommon`'s): `Record::put_field` plus the plan stamp. A record
    /// type's own link text (SIML, SIOL) is changed only through here.
    pub(crate) fn put_declared_field(&mut self, field: &str, value: EpicsValue) -> CaResult<()> {
        self.invalidate_plan_for(field);
        self.debug_assert_not_slotted(field, &value);
        let put = self.record.put_field(field, value);
        self.refresh_dol_mode(field);
        put
    }

    /// A `.db` field merged onto this installed record by a second
    /// `record(...)` block: the slot-aware funnel for a link, the declared
    /// funnel for anything else.
    pub(crate) fn put_merged_field(&mut self, field: &str, value: EpicsValue) -> CaResult<()> {
        match PreparedLink::for_put(self.record.field_list(), field, &value) {
            Some(link) => self.put_link_field(link, value),
            None => self.put_declared_field(field, value),
        }
    }

    /// [`Self::put_declared_field`] for a link with a slot: the slot takes
    /// `link`, parsed before the hold, only once the record stored the text.
    pub(crate) fn put_link_field(&mut self, link: PreparedLink, value: EpicsValue) -> CaResult<()> {
        let field = link.field();
        self.invalidate_plan_for(field);
        self.record.put_field(field, value)?;
        self.link_slots.install(link);
        Ok(())
    }

    /// [`Self::put_internal_field`] for one put of a batch whose link parses
    /// were made before the hold; the link field it installed, if any, which
    /// the caller opens
    /// ([`PvDatabase::put_internal_field_opening`](crate::server::database::PvDatabase)).
    pub(crate) fn put_internal_field_of(
        &mut self,
        field: &str,
        value: EpicsValue,
        links: &mut PreparedLinks,
    ) -> CaResult<Option<&'static str>> {
        let Some(link) = links.take(field) else {
            return self.put_internal_field(field, value).map(|()| None);
        };
        let field = link.field();
        self.invalidate_plan_for(field);
        self.record.put_field_internal(field, value)?;
        self.link_slots.install(link);
        Ok(Some(field))
    }

    /// A slotted link's text comes in through [`Self::put_link_field`]
    /// alone, or its slot goes stale.
    #[inline(always)]
    fn debug_assert_not_slotted(&self, field: &str, value: &EpicsValue) {
        debug_assert!(
            !matches!(value, EpicsValue::String(_)) || self.link_slots.position(field).is_none(),
            "{}.{field}: a link put must go through put_link_field",
            self.name
        );
    }

    /// Re-read `(OMSL, OIF)` after a put of either.
    fn refresh_dol_mode(&mut self, field: &str) {
        match field {
            "OMSL" => self.dol_mode.0 = menu_index_of(&*self.record, "OMSL"),
            "OIF" => self.dol_mode.1 = menu_index_of(&*self.record, "OIF"),
            "SELM" => self.selm = menu_index_of(&*self.record, "SELM"),
            _ => {}
        }
    }

    /// `(OMSL, OIF)` as the cycle's closed-loop DOL test reads them.
    #[inline(always)]
    pub(crate) fn dol_mode(&self) -> (i16, i16) {
        self.dol_mode
    }

    /// `SELM` as the cycle's NVL gate reads it.
    #[inline(always)]
    pub(crate) fn selm(&self) -> i16 {
        self.selm
    }

    /// This cycle's forward link, resolved from the two things C's
    /// `dbScanFwdLink` reads: the record's own veto
    /// ([`Record::should_fire_forward_link`]) and `FLNK`'s parsed kind.
    ///
    /// The single owner of that question. The DB half and the external half
    /// used to be derived at separate points — the DB name under the monitor
    /// segment's guard, the external PV by `dispatch_external_forward_link`
    /// re-taking the record's read lock in the tail — so every record in the
    /// database paid a second acquisition and a second `should_fire_forward_link`
    /// call once a cycle, and the two derivations could disagree about a
    /// record whose veto changed in between.
    #[inline(always)]
    pub(crate) fn forward_target(&self) -> ForwardTarget {
        // The link kind goes first: with nothing to fire (the common empty
        // FLNK) the record's veto is never asked, as in `dbScanFwdLink`.
        if self.parsed_flnk.is_constant() {
            return ForwardTarget::None;
        }
        match self.parsed_flnk.link() {
            // Unbound only before `iocInit`, when C's link is still a
            // PV_LINK with no lset to forward through.
            ParsedLink::Db(l) if self.record.should_fire_forward_link() => match l.forward() {
                Some(target) => ForwardTarget::Db {
                    target,
                    putf: self.common.putf,
                    notify: self.notify.is_some(),
                },
                None => ForwardTarget::None,
            },
            ParsedLink::Pva(_) | ParsedLink::PvaJson(_) | ParsedLink::Ca(_)
                if self.record.should_fire_forward_link() =>
            {
                ForwardTarget::External
            }
            // Constant / Hw / Calc / None carry no forward action.
            _ => ForwardTarget::None,
        }
    }

    /// The `FLNK` [`Self::forward_target`] reads.
    #[inline(always)]
    pub(crate) fn flnk(&self) -> &ParsedLink {
        self.parsed_flnk.link()
    }

    /// The parse of multi-input link `slot` for a reader that holds the
    /// record shared: the cached one when its text is the record's current
    /// text, a fresh parse otherwise — never written back, since the fetch
    /// stage that runs under the exclusive guard validates the entry itself.
    pub(crate) fn cached_multi_input(
        &self,
        slot: usize,
        link_field: &str,
    ) -> Option<Arc<ParsedLink>> {
        let owned;
        let text: &[u8] = match self.record.link_text_ref(link_field) {
            Some([]) => return None,
            Some(text) => text,
            None => {
                owned = self.link_text(link_field)?;
                owned.as_bytes()
            }
        };
        match self.parsed_inputs.get(slot) {
            Some(Some(entry)) if entry.text == text => Some(entry.parsed.clone()),
            _ => Some(Arc::new(parse_link_v2(text))),
        }
    }

    /// `has_const_lset` of [`Self::sim_link`]`(Siml)` without the slot
    /// lookup: the steady cycle's SIML gate.
    #[inline(always)]
    pub(crate) fn siml_has_const_lset(&self) -> bool {
        self.link_slots.siml_has_const_lset()
    }

    /// SIM-block link `which` (`SIML` or `SIOL`) as its slot holds it: the
    /// parse made at load and on each put, which C makes at `dbInitLink` and
    /// on each put of the field, and [`ParsedLink::Removed`] once a refused
    /// put removed it. The slot's type is the field's, so SIOL carries the
    /// `DBF_INLINK` mask on an input type and the `DBF_OUTLINK` one on an
    /// output type.
    #[inline(always)]
    pub(crate) fn sim_link(&self, which: SimLink) -> &Arc<ParsedLink> {
        self.cycle_link(which.cycle_link())
    }

    /// Whether `field` is a link with a slot — the load's check that a
    /// [`Self::record_link`] consumer names one.
    pub(crate) fn has_link_slot(&self, field: &str) -> bool {
        self.link_slots.position(field).is_some()
    }

    /// The parse of record-type link field `field` as the record holds it —
    /// [`LinkSlots`], made at load and on each put, so a cycle borrows it
    /// under the read lock and does no parse. A DB link's target address is
    /// the parse's own ([`PvDatabase::db_link_addr`]), so a cycle reading
    /// through this does no name work. Not for a `dbCommon` link the record
    /// leaves to the framework, which has its [`LinkCache`].
    ///
    /// [`PvDatabase::db_link_addr`]: crate::server::database::PvDatabase
    #[inline]
    pub(crate) fn record_link(&self, field: &str) -> &Arc<ParsedLink> {
        let Some(i) = self.link_slots.position(field) else {
            debug_assert!(false, "{}.{field}: not a link its type declares", self.name);
            return LinkSlots::none();
        };
        self.slot_link(i)
    }

    /// [`Self::record_link`] for a [`CycleLink`], by the slot settled at
    /// construction; no link when the type does not declare it.
    #[inline(always)]
    pub(crate) fn cycle_link(&self, link: CycleLink) -> &Arc<ParsedLink> {
        match LinkSlots::cycle_slot(self.cycle_links, link) {
            Some(i) => self.slot_link(i),
            None => LinkSlots::none(),
        }
    }

    /// Slot `i`'s parse, checked in debug builds against the text the record
    /// holds.
    #[inline]
    fn slot_link(&self, i: usize) -> &Arc<ParsedLink> {
        #[cfg(debug_assertions)]
        {
            // The lent text, so a checked cycle allocates no more than a
            // release one; a type that lends none, or an unset link the
            // override store may hold, takes the owned read.
            let field = self.link_slots.field_at(i);
            let held = match self.record.link_text_ref(field) {
                Some(text) if !text.is_empty() => Cow::Borrowed(text),
                _ => Cow::Owned(self.link_text(field).unwrap_or_default().into_bytes()),
            };
            debug_assert_eq!(
                held,
                self.link_slots.text_at(i),
                "{}.{field}: link text changed without a put through \
                 put_declared_field",
                self.name
            );
        }
        self.link_slots.at(i)
    }

    /// The link the record holds in its own link field `field`: the slot, or
    /// the `dbCommon` link's [`LinkCache`]; `None` for a field that is not
    /// one of the record's links. A cycle or put path that acts on the
    /// record's own link reads it here and never re-parses the text, which
    /// a removed link keeps ([`ParsedLink::Removed`]).
    pub(crate) fn stored_link(&self, field: &str) -> Option<&ParsedLink> {
        self.stored_link_shared(field).map(|link| &**link)
    }

    /// [`Self::stored_link`] as the shared parse, which an output write
    /// keeps past a swap of the field.
    pub(crate) fn stored_link_shared(&self, field: &str) -> Option<&Arc<ParsedLink>> {
        if let Some(link) = self.declared_record_link(field) {
            return Some(link);
        }
        match field {
            "INP" => Some(self.parsed_inp.shared()),
            "OUT" => Some(self.parsed_out.shared()),
            "TSEL" => Some(self.parsed_tsel.shared()),
            "SDIS" => Some(self.parsed_sdis.shared()),
            "FLNK" => Some(self.parsed_flnk.shared()),
            _ => None,
        }
    }

    /// [`Self::record_link`] for a field the type may not declare: `None`
    /// when it is not one of this type's links.
    pub(crate) fn declared_record_link(&self, field: &str) -> Option<&Arc<ParsedLink>> {
        self.link_slots
            .position(field)
            .map(|i| self.link_slots.at(i))
    }

    /// Link `i` of the type's output row (fanout/seq `LNKn`, dfanout `OUTn`),
    /// read by slot index. The load refused a record of these types whose
    /// row is not all slotted links ([`Self::has_link_slot`]).
    #[inline]
    pub(crate) fn out_row_link(&self, i: usize) -> &Arc<ParsedLink> {
        self.row_link(self.link_rows.0, i)
    }

    /// The field link `i` of the output row lives in.
    pub(crate) fn out_row_field(&self, i: usize) -> &'static str {
        match self.link_rows.0 {
            Some(row) => self.link_slots.field_at(LinkSlots::row_slot(row, i)),
            None => "",
        }
    }

    /// Link `i` of seq's `DOLn` row; see [`Self::out_row_link`].
    #[inline]
    pub(crate) fn dol_row_link(&self, i: usize) -> &Arc<ParsedLink> {
        self.row_link(self.link_rows.1, i)
    }

    fn row_link(&self, row: Option<LinkRow>, i: usize) -> &Arc<ParsedLink> {
        let Some(row) = row else {
            debug_assert!(false, "{}: no link row", self.name);
            return LinkSlots::none();
        };
        self.slot_link(LinkSlots::row_slot(row, i))
    }

    /// The text of a link field, `None` when the link is unset.
    ///
    /// A declared link is unset far more often than set — a `calc` declares 21
    /// inputs and a stock database wires none of them — and the process cycle
    /// asks every declared link on every pass, so the unset answer is the one
    /// that has to be cheap. Returning `Option` rather than an empty `String`
    /// is what makes it so at the call sites: there is no empty text to test,
    /// so nothing the caller wants only for a set link — a cloned field name,
    /// a parse, a `Vec` entry — can be built before the answer is known. Each
    /// of the six link reads this replaced did build something first and throw
    /// it away.
    ///
    /// A link the record leaves to the framework (`mca`'s SIML/SIOL) keeps
    /// its text in the override store, read here after the record.
    #[inline]
    pub(crate) fn link_text(&self, field: &str) -> Option<PvString> {
        link_text_of(&*self.record, field).or_else(|| self.override_link_text(field))
    }

    #[cold]
    fn override_link_text(&self, field: &str) -> Option<PvString> {
        if self.cold.declared_overrides.is_empty() {
            return None;
        }
        match self.cold.declared_overrides.get(field)? {
            EpicsValue::String(text) if !text.is_empty() => Some(text.clone()),
            _ => None,
        }
    }

    /// Whether link field `field` holds a text — [`Self::link_text`] without
    /// the copy.
    pub(crate) fn link_is_set(&self, field: &str) -> bool {
        match self.record.link_text_ref(field) {
            Some(text) => !text.is_empty(),
            None => self.link_text(field).is_some(),
        }
    }

    /// Store `PRIO` and the copy the record's [`DeviceDone`] reads without
    /// the record lock.
    ///
    /// [`DeviceDone`]: crate::server::database::DeviceDone
    pub fn set_prio(&mut self, prio: i16) {
        self.common.prio = prio;
        self.device_done.set_prio(prio);
    }

    pub fn new_boxed(name: impl Into<crate::types::RecordName>, record: Box<dyn Record>) -> Self {
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
        // See the field's own doc: fixed by the record TYPE, both sides of it.
        let link_backed_metadata_input_slot: Vec<Option<usize>> = link_backed_metadata_links
            .iter()
            .map(|lf| {
                record
                    .multi_input_links()
                    .iter()
                    .position(|(mf, _)| mf == lf)
            })
            .collect();
        let udf_alarm = record.raises_udf_alarm().then(|| UdfAlarm {
            exact_one: record.udf_alarm_on_exact_one(),
            severity: record.udf_alarm_severity(),
            message: record.udf_alarm_message(),
        });
        let monitor_plan = MonitorPlan::of(record.as_ref());
        let monitor_hot = MonitorHot::of(&monitor_plan);
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
            "ai" | "ao" | "calc" | "calcout" | "sub" | "scalcout" => {
                Some(AnalogAlarmConfig::double())
            }
            "longin" | "longout" => Some(AnalogAlarmConfig::long()),
            "int64in" | "int64out" => Some(AnalogAlarmConfig::int64()),
            _ => None,
        };
        let mut common = CommonFields::default();
        common.analog_alarm = analog_alarm;
        let multi_input_slots = record.multi_input_links().len();
        let link_slots = LinkSlots::of(record.as_ref());
        let cycle_links = link_slots.cycle_slots();
        let dol_mode = (
            menu_index_of(&*record, "OMSL"),
            menu_index_of(&*record, "OIF"),
        );
        let selm = menu_index_of(&*record, "SELM");
        let (out_row, dol_row) = crate::server::database::multi_out_rows(record.record_type());
        let link_rows = (
            out_row.and_then(|fields| link_slots.row(fields)),
            dol_row.and_then(|fields| link_slots.row(fields)),
        );

        Self {
            destroyed: false,
            device_done: crate::server::database::DeviceDone::detached(common.prio),
            name: name.into(),
            record,
            common,
            subscribers: smallvec::SmallVec::new(),
            parsed_inp: LinkCache::none(),
            parsed_out: LinkCache::none(),
            parsed_flnk: LinkCache::none(),
            parsed_sdis: LinkCache::none(),
            parsed_tsel: LinkCache::none(),
            parsed_inputs: (0..multi_input_slots).map(|_| None).collect(),
            simm_before_siml_read: None,
            link_slots,
            cycle_links,
            dol_mode,
            selm,
            link_rows,
            device: None,
            dpvt: Default::default(),
            subroutine: None,
            dset_init: crate::server::device_support::DsetInit::Ok,
            pact: AtomicBool::new(false),
            notify: None,
            notify_restart_list: std::collections::VecDeque::new(),
            last_posted: PostedLog::default(),
            posted: PostedSet::new(),
            udf_alarm,
            monitor_plan,
            array_hash_changed: false,
            monitor_hot,
            simpvt: false,
            suppress_subroutine_run: false,
            reprocess_generation: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            watchdog_generation: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            plan_rev: 1,
            cycle_plan: (0, CyclePlan::General, CycleFacts::default()),
            #[cfg(test)]
            plan_forced_general: false,
            #[cfg(test)]
            ran_arm: CyclePlan::General,
            cold: Box::new(RecordCold {
                cell: std::sync::Weak::new(),
                init_subroutines: None,
                db: Default::default(),
                io_intr_pump: None,
                property_drain: None,
                subroutine_pvt: None,
                skipped_posted: SkippedLog::default(),
                declared_overrides: HashMap::new(),
                link_backed_metadata_links,
                link_backed_metadata_input_slot,
                info: HashMap::new(),
                metadata_cache: MetadataCell::default(),
                alarm_reset: crate::server::recgbl::EventMask::NONE,
            }),
        }
    }

    /// Record what this cycle's `recGblResetAlarms` returned. See
    /// [`RecordCold::alarm_reset`].
    pub(crate) fn note_alarm_reset(&mut self, mask: crate::server::recgbl::EventMask) {
        self.cold.alarm_reset = mask;
    }

    /// Take what [`Self::note_alarm_reset`] recorded, leaving it empty.
    pub(crate) fn take_alarm_reset(&mut self) -> crate::server::recgbl::EventMask {
        std::mem::replace(
            &mut self.cold.alarm_reset,
            crate::server::recgbl::EventMask::NONE,
        )
    }

    /// The [`crate::server::record::ProcessAction::PutInLinkStage`] action.
    pub(crate) fn put_in_link_stage(
        &mut self,
        field: &str,
        value: crate::server::record::CycleValue,
        post: PostStage,
    ) {
        // C's `db_post_events` result is never read nor printed
        // (`scalerRecord.c:316`).
        if self.put_internal_field(field, value.into()).is_err() {
            return;
        }
        // Snapshot-cache contract, as `post_fields_with_mask`: rebuild
        // before the monitor snapshot is built.
        self.notify_field_written(field);
        if post == PostStage::Output {
            self.notify_field(field, crate::server::recgbl::EventMask::VALUE);
        }
    }

    /// The [`crate::server::record::ProcessAction::PutAfterEpilogue`] action: store
    /// `field` and post it with `DBE_VALUE | reset`, `reset` being this cycle's
    /// `recGblResetAlarms` return.
    pub(crate) fn put_after_epilogue(
        &mut self,
        field: &str,
        value: crate::server::record::CycleValue,
        reset: crate::server::recgbl::EventMask,
    ) {
        if self.put_internal_field(field, value.into()).is_ok() {
            self.notify_field_written(field);
            self.notify_field(field, crate::server::recgbl::EventMask::VALUE | reset);
        }
    }

    /// Hand the record the function registry its pending init pass 1 resolves
    /// INAM/SNAM against. The creation sink's line; see
    /// [`RecordCold::init_subroutines`].
    pub(crate) fn arm_init_subroutines(&mut self, registry: Arc<HashMap<String, Arc<Subroutine>>>) {
        self.cold.init_subroutines = Some(registry);
    }

    pub(crate) fn set_record_db(&mut self, db: crate::server::database::RecordDb) {
        self.cold.db = db;
    }

    /// C `(*psubroutine)(prec)`, for INAM at init and SNAM in `do_sub`.
    pub(crate) fn call_subroutine(&mut self, sub: &Subroutine) -> CaResult<i64> {
        match sub {
            Subroutine::Plain(f) => f(&mut *self.record),
            Subroutine::Context(f) => {
                let mut ctx = crate::server::database::SubroutineContext {
                    name: &self.name,
                    db: &self.cold.db,
                    pvt: &mut self.cold.subroutine_pvt,
                };
                f(&mut *self.record, &mut ctx)
            }
        }
    }

    /// C's `if (!pdset) { recGblRecordError(S_dev_noDSET, prec, "init_record");
    /// return S_dev_noDSET; }` — whether this record's `init_record` gets past
    /// its own first statement.
    ///
    /// Three terms, each one C's:
    /// * [`crate::server::recgbl::dev_sup_refusal`] IS the set of record types
    ///   whose `init_record` opens with that test — the same table that
    ///   supplies the message, so the refusal and the return can never
    ///   disagree about which types make it.
    /// * a soft DTYP (`""`, `Soft Channel`, `Raw Soft Channel`, `Async Soft
    ///   Channel`) resolves to a dset C always links, so the test passes.
    ///   `""` counts because C's DTYP index 0 is the record type's FIRST
    ///   `device()` line, which for every soft record type is the soft
    ///   channel.
    /// * anything else needs a device the resolver produced. `None` after the
    ///   creation sink has run its bind is C's `pdevSup == NULL`.
    pub(crate) fn init_record_reaches_body(&self) -> bool {
        self.device.is_some()
            || self.common.dtyp.is_soft()
            || crate::server::recgbl::dev_sup_refusal(self.record.record_type()).is_none()
    }

    /// C `registryFunctionFind` inside `init_record` pass 1, for the record
    /// types that make it. Returns whether the pass reached its tail.
    ///
    /// Unarmed (`init_subroutines` is `None`) means no registry was handed
    /// over — an iocsh `dbLoadRecords` merge re-running the passes on a record
    /// that already resolved. Nothing to look up again, and C's tail is
    /// reached.
    fn resolve_init_subroutine(&mut self, name: &str, err: &dyn Fn(&str)) -> bool {
        match self.cold.init_subroutines.take() {
            Some(registry) => crate::server::ioc_app::wire_subroutine(self, name, &registry, err),
            None => true,
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
    /// `dbStaticLib.c:2653-2661`). The callers are the creation sink,
    /// [`crate::server::database::PvDatabase::add_loaded_record`], which takes
    /// the load and the record together so no path can init a half-loaded
    /// record and hands it to the `iocInit` drain, and the loader's merge into
    /// a live record. Both run the three halves as C's three whole-database
    /// walks: this one, then [`Self::run_add_record`], then
    /// [`Self::run_init_pass1`] against what
    /// [`crate::server::database::PvDatabase::read_init_links`] read.
    ///
    /// C `doInitRecord0`: the prologue and `init_record(prec, 0)`. Returns
    /// whether the record's `init_record` gets past its dset test, i.e.
    /// whether the two later walks have anything to do for it.
    pub(crate) fn run_init_pass0(&mut self, name: &str) -> bool {
        let _init = crate::server::database::InitPass::enter();
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
        // C `iocInit.c::doInitRecord0` binds `prec->dset` for every record
        // (`:529`), including one whose `init_record` returns S_dev_noDSET
        // below; an unbound class would still follow a later DTYP put.
        self.common.dtyp.bind(self.record.record_type());
        // C `<rec>Record.c::init_record`'s FIRST statement, for the 26 record
        // types that make it: `if (!pdset) { recGblRecordError(S_dev_noDSET,
        // …); return S_dev_noDSET; }` (`aiRecord.c:105-110`,
        // `aoRecord.c:107-110`, …). Everything below it — `ao`'s `prec->init =
        // TRUE`, `ai`'s and `sub`'s MLST/ALST/LALM seed, `mbbo`'s SDEF fold —
        // is unreachable for a record whose dset is NULL, and running the
        // passes anyway is what left those cells set where softIoc reads 0.
        //
        // It is the OWNER's test, not each record's, for one structural
        // reason: `Record::init_record` is on the record BODY, which cannot
        // see `RecordInstance::device`. Asking every record type to re-derive
        // the answer is the per-cell patch this replaces.
        if !self.init_record_reaches_body() {
            return false;
        }
        if let Err(e) = self.record.init_record(0) {
            eprintln!("init_record(0) failed for {name}: {e}");
        }
        if self.record.dset_init_in_pass0() {
            crate::server::device_support::init_device_support(self);
        }
        true
    }

    /// C `doResolveLinks`' device-support half, for a record pass 0 reached.
    pub(crate) fn run_add_record(
        &mut self,
        name: &str,
        add_record_input: Option<crate::server::builtin_devices::soft_callback::AddRecordInput>,
    ) {
        let _init = crate::server::database::InitPass::enter();
        // C `iocInit.c::doResolveLinks` (`:545-570`), the pass BETWEEN the two
        // `init_record` calls: for each device link the record type declares,
        // `pdsxt->add_record(precord)` and then `dbInitLink` for that same
        // link. This is that call, and it keeps C's position — before the link
        // it complains about is initialised, after pass 0 has run.
        if let Some(refusal) =
            crate::server::builtin_devices::soft_callback::add_record(self, add_record_input)
        {
            refusal.report(name);
        }
    }

    /// C `doInitRecord1`: `init_record(prec, 1)` and the init tail, for a
    /// record pass 0 reached.
    pub(crate) fn run_init_pass1(
        &mut self,
        name: &str,
        reads: &mut crate::server::record::InitLinkReads,
        err: &dyn Fn(&str),
    ) {
        let _init = crate::server::database::InitPass::enter();
        if let Err(e) = self.record.init_record_with_links(reads) {
            eprintln!("init_record(1) failed for {name}: {e}");
        }
        reads.apply(&mut self.common);
        // C `pdset->common.init_record(prec)` — the driver's own init, which
        // every record type calls from INSIDE `init_record` once the dset test
        // above has passed (`aiRecord.c:115-124`, `aoRecord.c:121-133`). It
        // ran after both passes and after the constant-link seed while device
        // support was attached by a whole-database pass; it is here now
        // because the dset is bound before the passes. Residual difference
        // from C, not closed by this move: C calls it BEFORE the record's own
        // tail (`prec->mlst = prec->val`) and the port's tail is inside
        // `init_record(1)`, so a driver `init` that defines VAL is still not
        // reflected in the trackers seeded a line earlier.
        crate::server::database::seed_record_constants(self);
        if !self.record.dset_init_in_pass0() {
            crate::server::device_support::init_device_support(self);
        }
        self.seed_metadata_cache();
        // C `subRecord.c:107-129` / `aSubRecord.c:139-160` — the INAM call and
        // the SNAM lookup, also inside pass 1. `false` is C's early return
        // (`S_db_BadSub`, or the empty-SNAM `pact = TRUE; return 0`), so the
        // tail below is skipped exactly where C skips it.
        if !self.resolve_init_subroutine(name, err) {
            return;
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
        // C `init_record` that calls `recGblResetAlarms(prec)` (asyn, table).
        // The born `UDF`/`INVALID` (and the UDF-severity derivation above) are
        // overwritten here: at init `nsta`/`nsev` are 0, so
        // `rec_gbl_reset_alarms` transfers `STAT`/`SEVR` to `NO_ALARM`. UDF is
        // left to `post_init_finalize_undef`; table keeps UDF=1. Runs after it
        // so it is the final word on this record's initial alarm state.
        if self.record.init_resets_alarms() {
            let _ = crate::server::recgbl::rec_gbl_reset_alarms(&mut self.common);
        }
        // C `init_record` can park a record it cannot process with `prec->pact
        // = TRUE`. The only such line in base is `subRecord.c:119-123`, and it
        // sits WITH the empty-SNAM report and the return, so the resolution
        // step above performs it: a record whose INAM missed returned before
        // that line and is not parked however empty its SNAM is (softIoc reads
        // `PACT: 0`). This asks the record type for the same transition on the
        // paths that reach the tail — an iocsh `dbLoadRecords` merge re-running
        // the passes with no registry to resolve against. It is after the
        // passes so the `leave_pact()` above cannot undo it, and the park is
        // not permanent: a put to a `pact_park_fields()` field re-asks, the way
        // C's `special()` does.
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
    /// Whether this DTYP is a soft output dset at all — the question
    /// [`Self::soft_output_value`] answers `Some`/`None` to, asked without
    /// building the value.
    pub(crate) fn has_soft_output(&self) -> bool {
        self.common.dtyp.soft().is_some()
    }

    pub fn soft_output_value(&self) -> Option<Option<EpicsValue>> {
        use crate::server::device_support::SoftDtyp;
        match self.common.dtyp.soft()? {
            SoftDtyp::Raw => Some(
                self.record
                    .raw_soft_output_value()
                    .or_else(|| self.record.output_link_value()),
            ),
            SoftDtyp::Plain | SoftDtyp::Async => Some(self.record.output_link_value()),
        }
    }

    /// [`Self::soft_output_value`], borrowed: `Some(None)` is a soft dset
    /// whose value has no view, which the caller materialises.
    pub(crate) fn soft_output_view(&self) -> Option<Option<super::record_trait::OutView<'_>>> {
        use crate::server::device_support::SoftDtyp;
        match self.common.dtyp.soft()? {
            SoftDtyp::Raw => Some(self.record.raw_soft_output_view()),
            SoftDtyp::Plain | SoftDtyp::Async => Some(self.record.output_view()),
        }
    }

    /// Set a single `info("key", "value")` tag on this record. Last
    /// write wins. Used by the .db loader (`info(...)` directive) and
    /// `dbpf`-style tools.
    pub fn set_info(&mut self, key: impl Into<String>, value: impl Into<String>) {
        self.cold.info.insert(key.into(), value.into());
    }

    /// Every info tag of the record.
    pub fn info(&self) -> &HashMap<String, String> {
        &self.cold.info
    }

    /// Look up a single info tag. Returns `None` when the record has
    /// no tag with that key.
    pub fn get_info(&self, key: &str) -> Option<&str> {
        self.cold.info.get(key).map(|s| s.as_str())
    }

    /// The value of `field` already published to its `DBE_VALUE`/`DBE_LOG`
    /// subscribers, or `None` when the framework has never published one.
    /// A reference-log field's entry holds no value: C keeps no published
    /// copy of it. The read side of the `last_posted` contract — see the
    /// field's docs.
    pub(crate) fn posted_value(&self, field: &str) -> Option<&PostedValue> {
        self.posted_value_at(self.post_key(field))
    }

    /// [`Self::posted_value`] of the field filed under `key`.
    fn posted_value_at(&self, key: PostKey<'_>) -> Option<&PostedValue> {
        match self.last_posted.get(key) {
            Some(published) => Some(&published.value),
            None => self.cold.skipped_posted.get(key),
        }
    }

    /// The published-log key of `field`, by its name: the one name lookup of
    /// a name-addressed post.
    fn post_key<'a>(&self, field: &'a str) -> PostKey<'a> {
        #[cfg(test)]
        NAME_BYTE_COMPARES.with(|n| n.set(n.get() + 1));
        match declared_desc(self.field_desc(field), field) {
            Some(desc) => self.monitor_plan.key_of(desc),
            None => PostKey::Named(field),
        }
    }

    /// What a post of the field filed under `key` carries: nothing for a
    /// reference-log field, which is never read for it, else `read`. The kind
    /// is the published entry's, settled when the field was first published,
    /// and only a field never published asks its declaration (`desc`).
    fn post_payload_at(
        &self,
        key: PostKey<'_>,
        desc: impl FnOnce(&Self) -> Option<&'static FieldDesc>,
        read: impl FnOnce(&Self) -> Option<EpicsValue>,
    ) -> Option<PostedValue> {
        match self.posted_value_at(key) {
            Some(PostedValue::Ref) => Some(PostedValue::Ref),
            Some(PostedValue::Val(_)) => read(self).map(PostedValue::Val),
            None => match desc(self) {
                desc @ Some(_) if posts_by_ref(desc, || None) => Some(PostedValue::Ref),
                Some(_) => read(self).map(PostedValue::Val),
                // A type with no field table declares a field by its value.
                None => read(self).map(|v| {
                    if crate::server::event_queue::LogKind::of_value(&v)
                        == crate::server::event_queue::LogKind::Ref
                    {
                        PostedValue::Ref
                    } else {
                        PostedValue::Val(v)
                    }
                }),
            },
        }
    }

    /// [`Self::post_payload_at`] for a subscribed field the walk reaches before
    /// its first post, kept out of the walk's per-entry code.
    #[cold]
    #[inline(never)]
    fn first_post_payload(&self, key: PostKey<'_>, field: &str) -> Option<PostedValue> {
        self.post_payload_at(key, |i| i.field_desc(field), |i| i.resolve_field(field))
    }

    /// Whether `collect_subscriber_posts`'s walk discards `field` before it
    /// change-detects it, for every cycle of this record's life
    /// ([`walk_skip_term`]). The terms answer from `&'static` record metadata
    /// alone, so the answer cannot change under a field once it has been
    /// stored. `Record::process_posted_fields` is not here: it is an allow
    /// list whose membership turns over as the record publishes, so it stays a
    /// per-cycle term in the walk.
    fn walk_skips(&self, field: &str) -> bool {
        #[cfg(test)]
        NAME_BYTE_COMPARES.with(|n| n.set(n.get() + 1));
        walk_skip_term(
            field,
            self.record.monitor_deadband_field(),
            self.record.event_posted_fields(),
        )
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
    ///
    /// The deadband field is the exception: its published value is MLST/ALST,
    /// which [`Self::deadband_post`] owns, so a post of it stores nothing. So
    /// is a field the type claims ([`MonitorPlan::claimed`]): its `monitor()`
    /// decides from the record's own previous copies, as C's does.
    pub(crate) fn record_value_post(&mut self, field: &str, value: PostedValue) {
        self.record_value_post_as(self.post_key(field), field, value);
    }

    /// The record's [`Record::process_posted_fields`], with every
    /// `last_posted` entry's `posted` settled against it.
    #[inline]
    fn settle_posted(&mut self) -> Option<DeclaredFields> {
        let list = match self.monitor_plan.posts_closed {
            true => self.record.process_posted_fields(),
            false => None,
        };
        debug_assert_eq!(
            list.is_some(),
            self.record.process_posted_fields().is_some(),
            "{} opened or closed its process post set after the plan was built",
            self.record.record_type()
        );
        if !self.posted.is_settled(list) {
            self.resettle_posted(list);
        }
        list
    }

    #[cold]
    #[inline(never)]
    fn resettle_posted(&mut self, list: Option<DeclaredFields>) {
        debug_assert!(
            !self.monitor_plan.answers_own_posts || self.monitor_plan.own_posts_hold(&*self.record),
            "a turned-over post set broke answers_own_posts"
        );
        self.posted.resettle(list, &mut self.last_posted);
        self.settle_sub_walk();
    }

    /// Every entry's and monitor's list [`Roles`] settled against `lists`.
    #[inline(always)]
    fn settle_roles(&mut self, lists: Lists) {
        if !self.posted.lists_settled(&lists) {
            self.resettle_roles(lists);
        }
    }

    #[cold]
    #[inline(never)]
    fn resettle_roles(&mut self, lists: Lists) {
        let monitors = self
            .subscribers
            .iter_mut()
            .map(|sub| (sub.member(), &mut sub.roles));
        self.posted
            .resettle_lists(lists, &mut self.last_posted, monitors);
    }

    /// [`Self::record_value_post`] of `field`, filed under `key`. A declared
    /// field is answered from the plan's bits and found by index, so a post
    /// reads no name; only its first publish asks the record about it.
    fn record_value_post_as(&mut self, key: PostKey<'_>, field: &str, value: PostedValue) {
        if self.monitor_plan.stores_nothing(key) {
            #[cfg(test)]
            STORELESS_POSTS.with(|p| p.borrow_mut().push(field.to_string()));
            return;
        }
        if let Some(slot) = self.last_posted.get_mut(key) {
            slot.value = value;
            return;
        }
        let walk_skip = match key {
            PostKey::Ix(ix) => self.monitor_plan.walk_skip_bits.has(ix),
            PostKey::Named(name) => self.walk_skips(name),
        };
        if walk_skip {
            self.cold.skipped_posted.set(key, value);
            return;
        }
        let (desc, member) = match key {
            PostKey::Ix(ix) => (self.monitor_plan.desc_at(ix), Some(ix)),
            PostKey::Named(_) => (self.field_desc(field), None),
        };
        let published = self.posted.publish(
            field,
            value,
            desc,
            member,
            self.monitor_plan.fixed_roles(key),
        );
        self.last_posted.insert(key, published);
        self.monitor_hot.set(MonitorHot::PUBLISHED, true);
        if !self.unwatched() {
            self.settle_sub_walk();
        }
    }

    /// **The** writer of the metadata cache, called after a write of any
    /// metadata-class field (EGU, PREC, HOPR/LOPR, alarm limits, DRVH/DRVL,
    /// enum strings, DESC): a cache already built is rebuilt now rather than
    /// cleared, so a reader that cannot rebuild it — one inside this record's
    /// own processing — never finds it stale. One not yet built stays
    /// unbuilt; `seed_metadata_cache` builds it once init has written
    /// the record.
    pub fn refresh_metadata_cache(&self) {
        if self.cold.metadata_cache.load().is_some() {
            self.seed_metadata_cache();
        }
    }

    /// Build the metadata cache from the record as it stands: the end of
    /// `init_record`, after device support's `init` has written its fields
    /// (asyn's `setEnums`) with no notification.
    pub(crate) fn seed_metadata_cache(&self) -> Arc<MetadataSnapshot> {
        let meta = Arc::new(self.build_metadata());
        self.cold.metadata_cache.store(Some(Arc::clone(&meta)));
        meta
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
    pub(crate) fn field_posts_property(desc: Option<&'static FieldDesc>) -> bool {
        desc.is_some_and(|d| d.prop)
    }

    /// Hook called by the database after a field is written. If the field is a
    /// metadata-cache source, the cache is rebuilt from the new value. Posts nothing — a caller that also owes the
    /// `DBE_PROPERTY` event uses [`Self::notify_field_written_if_changed`].
    ///
    /// Field name is automatically uppercased.
    pub fn notify_field_written(&self, field: &str) {
        let upper = upper_field(field);
        if is_metadata_cache_source(&upper) {
            self.refresh_metadata_cache();
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
    pub fn notify_field_written_if_changed(&mut self, field: &str, prev: Option<&EpicsValue>) {
        let upper = upper_field(field);
        let desc = self.field_desc(&upper);
        self.written_if_changed(None, &upper, is_metadata_cache_source(&upper), desc, prev);
    }

    /// [`Self::notify_field_written_if_changed`] for a settled put target;
    /// `field` is upper-case.
    pub(crate) fn notify_field_written_if_changed_at(
        &mut self,
        burst: &mut PostBurst,
        field: &str,
        at: &PutAddr,
        pre: PrePut,
    ) {
        let PrePut(Some(prev)) = pre else {
            return;
        };
        self.written_if_changed(
            Some(burst),
            field,
            at.cache_source,
            at.field.desc,
            prev.as_ref(),
        );
    }

    /// The pre-put value of `field` at `at` for the change test; a field the
    /// test does not watch is not read.
    pub(crate) fn pre_put_at(&self, field: &str, at: &PutAddr) -> PrePut {
        PrePut(
            at.watched
                .then(|| self.resolve_field_stored_at(field, at.field.desc)),
        )
    }

    /// `desc` is `upper`'s declaration ([`Self::field_desc`]).
    fn written_if_changed(
        &mut self,
        burst: Option<&mut PostBurst>,
        upper: &str,
        cache_source: bool,
        desc: Option<&'static FieldDesc>,
        prev: Option<&EpicsValue>,
    ) {
        let posts_property = Self::field_posts_property(desc);
        if !cache_source && !posts_property {
            return;
        }
        // The SAME reader the put's pre-value was captured with
        // (`field_io.rs`'s three `dbPut` bodies). `Record::get_field` alone
        // sees only the record type's own struct, and this port keeps on
        // `CommonFields` a good deal of storage C keeps per record type — the
        // whole analog-alarm ladder among it — so a `caput HIHI` on a calc
        // compared `None` to `None`, reported "unchanged", and posted neither
        // the `DBE_PROPERTY` C sends for a `prop(YES)` field nor the cache
        // invalidation the metadata it feeds depends on.
        let now = self.resolve_field_stored_at(upper, desc);
        if prev == now.as_ref() {
            return;
        }
        if cache_source {
            self.refresh_metadata_cache();
        }
        if posts_property {
            // mirror C dbAccess.c:1395-1396 — the gate `if (propertyUpdate &&
            // !status)` and the `db_post_events(precord, NULL, DBE_PROPERTY)` it
            // guards. The NULL field pointer is what makes it record-wide.
            let mut own;
            let burst = match burst {
                Some(burst) => burst,
                None => {
                    own = PostBurst::new();
                    &mut own
                }
            };
            self.post_record_wide(burst, crate::server::recgbl::EventMask::PROPERTY);
        }
    }

    /// Returns the cached MetadataSnapshot, building and storing it on
    /// the first call, so the populate cost is paid at most once per
    /// metadata-stable interval.
    fn cached_metadata(&self) -> MetadataSnapshot {
        match self.cold.metadata_cache.load_full() {
            Some(cached) => (*cached).clone(),
            None => (*self.seed_metadata_cache()).clone(),
        }
    }

    fn build_metadata(&self) -> MetadataSnapshot {
        #[cfg(test)]
        METADATA_BUILDS.with(|n| n.set(n.get() + 1));
        let mut tmp = super::super::snapshot::Snapshot::new(
            EpicsValue::Double(0.0),
            0,
            0,
            std::time::SystemTime::UNIX_EPOCH,
        );
        self.populate_display_info(&mut tmp);
        self.populate_control_info(&mut tmp);
        self.populate_enum_info(&mut tmp);
        MetadataSnapshot {
            display: tmp.display,
            control: tmp.control,
            enums: tmp.enums,
            alarm: self.explicit_alarm_limits(self.record.record_type()),
        }
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
        declared_no_mod(field, self.field_desc(field)) || self.record.field_no_mod(field)
    }

    /// [`Self::is_no_mod`] for a settled put target.
    pub(crate) fn is_no_mod_at(&self, field: &str, at: &PutAddr) -> bool {
        at.no_mod || self.record.field_no_mod(field)
    }

    /// Check if the record is currently processing (PACT equivalent).
    pub fn is_processing(&self) -> bool {
        self.pact.load(std::sync::atomic::Ordering::Acquire)
    }

    /// [`Record::set_process_continuation`], handed this instance's PACT.
    #[inline(always)]
    pub(crate) fn set_process_continuation(&mut self, continuation: bool) {
        if self.has_hook(crate::server::record::CycleHooks::CONTINUATION) {
            self.record.set_process_continuation(
                continuation,
                crate::server::record::Pact::new(&self.pact),
            );
        }
    }

    /// Whether the type answers `hook` with anything but the trait default;
    /// the wrappers below ask the record only then.
    #[inline(always)]
    fn has_hook(&self, hook: crate::server::record::CycleHooks) -> bool {
        self.monitor_plan.hooks.has(hook)
    }

    /// [`Record::set_fetch_gate_failed`].
    #[inline(always)]
    pub(crate) fn set_fetch_gate_failed(&mut self, failed: bool) {
        if self.has_hook(crate::server::record::CycleHooks::INPUT_FETCH) {
            self.record.set_fetch_gate_failed(failed);
        }
    }

    /// [`Record::set_resolved_input_links`].
    #[inline(always)]
    pub(crate) fn set_resolved_input_links(
        &mut self,
        resolved: crate::server::record::ResolvedInputLinks<'_>,
    ) {
        if self.has_hook(crate::server::record::CycleHooks::INPUT_FETCH) {
            self.record.set_resolved_input_links(resolved);
        }
    }

    /// [`Record::read_constant_inp`] of this record's INP.
    #[inline(always)]
    pub(crate) fn read_constant_inp(&mut self) -> bool {
        self.has_hook(crate::server::record::CycleHooks::INPUT_FETCH)
            && self.record.read_constant_inp(self.parsed_inp.link())
    }

    /// [`Record::should_output`].
    #[inline(always)]
    pub(crate) fn should_output(&self) -> bool {
        !self.has_hook(crate::server::record::CycleHooks::OUTPUT_HOOKS)
            || self.record.should_output()
    }

    /// [`Record::output_event`].
    #[inline(always)]
    pub(crate) fn output_event(&self) -> Option<crate::types::PvString> {
        if self.has_hook(crate::server::record::CycleHooks::OUTPUT_HOOKS) {
            self.record.output_event()
        } else {
            None
        }
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
    /// The slot alone decides, not the restart list. A
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
        self.take_notify_slot(notify.clone());
        Some(notify)
    }

    /// Take `ws` into this record's put-notify slot.
    ///
    /// **The only writer of [`Self::notify`] that installs a set**; every other
    /// write only clears it. Everything that makes a record a member of a wait-set happens here,
    /// so C's `precord->ppn = ppn` and its `ellSafeAdd(&waitList, ...)`
    /// (`dbNotify.c:226-227`, `:257-258`, `:497-498`) stay the single act they
    /// are in C. Split across two call sites, they were what let a member exist
    /// that no cancel could name.
    fn take_notify_slot(&mut self, ws: Arc<NotifyWaitSet>) {
        debug_assert!(
            self.notify.is_none(),
            "the slot must be tested free in the same critical section"
        );
        ws.record_joined(&self.name);
        self.notify = Some(ws);
    }

    /// C `dbNotifyAdd` (dbNotify.c:477-501): a link target joins the wait-set
    /// of the put-notify driving the chain, so the initiator's completion
    /// waits for this record's cycle too.
    ///
    /// One of the two callers of `take_notify_slot`, the sole writer; the
    /// other is [`Self::install_or_queue_notify`]. All three live here so
    /// the slot has no assignment site outside this module — an open-coded one
    /// elsewhere is how a wait-set came to be installed without the record's
    /// write gate.
    ///
    /// A record already carrying a wait-set keeps it (C's `if (!pto->ppn …)`
    /// at `:492`), so this never displaces a live one, and the `enter` is
    /// paired with the `leave` the target's own cycle tail performs.
    pub fn join_put_notify(&mut self, src: Option<&Arc<NotifyWaitSet>>) {
        if self.notify.is_some() {
            return;
        }
        if let Some(ws) = src {
            ws.enter();
            self.take_notify_slot(ws.clone());
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

    /// The set in this record's slot if it can never be answered — the test
    /// half of C `dbNotifyCancel` (`dbNotify.c:385-430`), reached from
    /// `rsrvFreePutNotify` (`camessage.c:1630-1638`) when a client is torn down
    /// with its put-callback still busy.
    ///
    /// # Invariant (CONTRACT)
    ///
    /// A record's put-notify slot MUST NOT stay occupied by a notify nobody
    /// can be answered from. Without the release such a record is wedged for
    /// good: every later put-notify queues on `notify_restart_list` behind a
    /// completion that can never arrive and writes nothing — C
    /// `processNotifyCommon` tests ownership ABOVE `putCallback` — so the next
    /// client hangs too, and the one after it.
    ///
    /// This is what makes honouring a record type's forward-link gate safe at
    /// all. `busy` left at VAL=1 withholds the `ca_put_callback` exactly as C
    /// does (`busyRecord.c:271`); the client then gives up and exits, and that
    /// exit is the teardown modelled here.
    ///
    /// Two triggers reach it, and both are needed. The CA server calls it at
    /// client teardown, which is C's own moment
    /// (`rsrvFreePutNotify`, `camessage.c:1630-1638`), and it also runs at
    /// every ownership test, which catches a set whose client died on a
    /// transport the teardown hook does not cover. Only the first closes the
    /// case where a SECOND client is already queued on a record's restart list
    /// when the first dies: nothing then arrives to test ownership, and C's
    /// `restartCheck` would have handed that record to the queued client.
    ///
    /// The set names its own members ([`NotifyWaitSet::joined_records`]), so
    /// the sweep reaches a chain target exactly as C's `notifyProcessInProgress`
    /// arm does (`dbNotify.c:428-430`) rather than stopping at the entry.
    /// That distinction is not cosmetic: a chain target is the likelier victim,
    /// because the record whose cycle never ends is precisely the one this
    /// exists for — a `busy` left at VAL=1 withholds `recGblFwdLink` by
    /// contract, so its slot would otherwise be held by a dead set forever.
    ///
    /// See [`PvDatabase::cancel_unanswerable_notify`], the owner that runs it
    /// across the whole set.
    ///
    /// [`PvDatabase::cancel_unanswerable_notify`]: crate::server::database::PvDatabase::cancel_unanswerable_notify
    pub(crate) fn unanswerable_notify(&self) -> Option<Arc<NotifyWaitSet>> {
        self.notify
            .as_ref()
            .filter(|ws| ws.is_unanswerable())
            .cloned()
    }

    /// Drop this record's claim on `dead` — C `restartCheck`'s
    /// `precord->ppn = 0` (`dbNotify.c:157`) as `dbNotifyCancel` reaches it.
    ///
    /// The record's own slot is the authority on membership, so this is
    /// `Arc::ptr_eq`-gated: a name the sweep carries for a record that has
    /// since completed and taken a different notify releases nothing.
    ///
    /// Returns whether the slot was freed, so the caller can promote the
    /// restart-list head (C `restartCheck`) exactly as a completion would.
    #[must_use = "a freed slot owes the restart list a drain"]
    pub(crate) fn release_notify(&mut self, dead: &Arc<NotifyWaitSet>) -> bool {
        if self.notify.as_ref().is_some_and(|ws| Arc::ptr_eq(ws, dead)) {
            self.notify = None;
            return true;
        }
        false
    }

    /// Whether a put-notify owns this record — C `precord->ppn != NULL`.
    ///
    /// The public read of the slot. The wait-set itself stays crate-private so
    /// no caller outside this crate can `enter`/`leave` a set it does not own,
    /// which is the accounting [`NotifyWaitSet`] exists to keep.
    pub fn has_notify(&self) -> bool {
        self.notify.is_some()
    }

    /// Whether a restarted put-notify waits behind this record — C
    /// `precord->ppnr != NULL`, what `dbpr` prints as `PPNR`.
    pub(crate) fn has_notify_restart(&self) -> bool {
        !self.notify_restart_list.is_empty()
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
    /// **The only drain** of a live record ([`Self::destroy`] discards the
    /// list of a removed one). The freedom test lives here rather than at the call
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
        self.resolve_field_upper(&upper_field(name))
    }

    /// [`Self::resolve_field`] for a name the caller has already normalised
    /// — a parsed link's target field, a channel name's field — so the link
    /// read path does not allocate an upper-cased copy of a name that is
    /// upper-case by construction. `name` must already be upper-case.
    pub fn resolve_field_upper(&self, name: &str) -> Option<EpicsValue> {
        debug_assert!(
            !name.bytes().any(|b| b.is_ascii_lowercase()),
            "resolve_field_upper takes a normalised name, got {name:?}"
        );
        self.resolve_field_upper_at(name, self.field_desc(name))
    }

    /// [`Self::resolve_field_upper`] with the field's declaration in hand:
    /// `desc` is [`Self::field_desc`]'s answer for `name`, which a link
    /// target settles once ([`FieldAddr`]) instead of scanning per read.
    #[inline]
    pub(crate) fn resolve_field_upper_at(
        &self,
        name: &str,
        desc: Option<&'static FieldDesc>,
    ) -> Option<EpicsValue> {
        let value = self.resolve_field_stored_at(name, desc)?;
        Some(self.as_a_reader_sees(desc, value))
    }

    /// [`Self::resolve_field_upper_at`] of the field declared `desc`, through
    /// the type's slot when it maps one — C reads through the `dbAddr` — so
    /// no name is matched.
    pub(crate) fn read_declared(&self, desc: &'static FieldDesc) -> Option<EpicsValue> {
        self.resolve_field_upper_at(desc.name, Some(desc))
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
        self.resolve_field_stored_at(name, self.field_desc(name))
    }

    /// [`Self::resolve_field_stored`] with `desc` = [`Self::field_desc`]`(name)`.
    #[inline]
    pub(crate) fn resolve_field_stored_at(
        &self,
        name: &str,
        desc: Option<&'static FieldDesc>,
    ) -> Option<EpicsValue> {
        let value = match desc {
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
            Some(desc) => match self.monitor_plan.read_of(desc) {
                FieldRead::Slot(slot) => {
                    match self.record.slot_ref(crate::server::record::FieldSlot(slot)) {
                        Some(slot) => slot.to_value(desc.dbf_type),
                        None => self.read_named(name)?,
                    }
                }
                FieldRead::Common(field) => match self.common_field(field) {
                    Some(value) => value,
                    None => self.read_named(name)?,
                },
                FieldRead::Named => self.read_named(name)?,
                FieldRead::Unreadable => return None,
            },
            // C `dbNameToAddr` falls through to `dbGetAttributePart` on
            // `S_dbLib_fieldNotFound` (`dbAccess.c:672-675`), which is how
            // `RTYP` — declared by no record type — reads as the type name.
            // `VERS` and any `dbPutAttribute` name need the database's
            // attribute map and are answered a level up, in `get_pv`.
            None => {
                Self::count_name_read();
                self.get_virtual_field(name)?
            }
        };
        Some(value)
    }

    /// The declared field `name` by the name chain, out of line so the
    /// settled routes keep a small frame.
    #[inline(never)]
    fn read_named(&self, name: &str) -> Option<EpicsValue> {
        // C `dbFindFieldPart` — the record type's own `.dbd` table, then
        // `dbCommon`. Every accessor below reads STORAGE, and this port
        // keeps a good deal of storage on `CommonFields` that C keeps per
        // record type (`INP`, `OUT`, `SSCN`, the analog-alarm ladder), so
        // without the declaration in front of them a `calc` answered
        // `.OUT` with an empty string where C answers `PV 'C:GOOD.OUT'
        // not found`. The declaration is the namespace, not the storage.
        //
        // The record's own answer is returned as it lands rather than
        // threaded through the fallbacks' `or_else` chain, which moved
        // the 32-byte value once per link in the chain on the way out.
        match self.record_get_field_by_name(name) {
            Some(value) => Some(value),
            None => self
                .get_common_field(name)
                .or_else(|| self.get_virtual_field(name))
                .or_else(|| self.cold.declared_overrides.get(name).cloned())
                .or_else(|| self.declared_default(name)),
        }
    }

    /// The name chain's entry: the record's own `get_field` by name.
    fn record_get_field_by_name(&self, name: &str) -> Option<EpicsValue> {
        Self::count_name_read();
        self.record.get_field(name)
    }

    #[inline(always)]
    fn count_name_read() {
        #[cfg(test)]
        {
            NAME_READS.with(|n| n.set(n.get() + 1));
            NAME_BYTE_COMPARES.with(|n| n.set(n.get() + 1));
        }
    }

    /// C `dbGet`'s link arm applied to one resolved field: a link field reads
    /// as [`render_link_field`], everything else as itself.
    ///
    /// The class is read off `desc`, the declaration the caller resolved the
    /// field with, so a string read pays no second scan for it.
    #[inline]
    fn as_a_reader_sees(&self, desc: Option<&'static FieldDesc>, value: EpicsValue) -> EpicsValue {
        let EpicsValue::String(ref text) = value else {
            return value;
        };
        let Some((class, desc)) = desc.and_then(|d| Some((crate::types::link_class_of(d)?, d)))
        else {
            return value;
        };
        let text = text.as_bytes();
        let rendered = match self.declared_record_link(desc.name).map(|l| &**l) {
            Some(ParsedLink::Removed(ty)) => render_removed_link(class, *ty, text),
            _ if desc.name == "INP" && self.inp_is_pn_link() => render_pn_link(text),
            _ if matches!(desc.name, "INP" | "OUT")
                && super::declared_link_type(
                    self.record.record_type(),
                    Some(self.common.dtyp.as_str()),
                    desc.name,
                ) == Some(super::DbLinkType::PnLink) =>
            {
                render_pn_link(text)
            }
            _ => render_link_field(class, text),
        };
        EpicsValue::String(rendered)
    }

    /// C `!dbLinkIsDefined(plink)` for a link a runtime put removed: a
    /// device link's failed put leaves the record with no dset
    /// ([`Dtyp::is_detached`](crate::server::device_support::Dtyp::is_detached)),
    /// and any other link is [`ParsedLink::Removed`] in its slot.
    pub(crate) fn link_unadded(&self, field: &str) -> bool {
        if self.is_device_link(field) {
            return self.common.dtyp.is_detached();
        }
        self.link_slots
            .position(field)
            .is_some_and(|i| self.link_slots.is_removed(i))
    }

    /// The old dsxt's `del_record` on a device-link put (`dbAccess.c:1163`):
    /// a SoftCallback devPvt is cancelled and freed
    /// (`devAiSoftCallback.c:138-140`), so a pending completion re-entry of
    /// this record goes with it. Any other `dpvt` stays until `:1192`'s
    /// `dpvt = NULL`, which only a put that reaches `add_record` passes.
    fn del_record(&mut self) -> crate::server::device_support::Dpvt {
        if !self.inp_is_pn_link() {
            return crate::server::device_support::Dpvt::Null;
        }
        self.reprocess_generation
            .fetch_add(1, std::sync::atomic::Ordering::AcqRel);
        std::mem::take(&mut self.dpvt)
    }

    /// C `doCloseLinks`' device half (`iocInit.c:689-699`): a bound dset with
    /// a `dsxt` leaves its scan list and its `del_record` runs, whose status C
    /// ignores here. True when it ran, so the caller takes PACT as C does.
    pub(crate) fn close_device(&mut self) -> bool {
        use crate::server::device_support::{BoundDsxt, IoIntrCmd};
        if self.common.dtyp.bound_dsxt() != BoundDsxt::Has {
            return false;
        }
        // `scanDelete` (`dbScan.c:240-262`). Only an I/O Intr list's leave
        // reaches the device; a periodic or event list is not left here, and
        // PACT plus the scan stop that follows end its scans.
        if self.common.scan == ScanType::IoIntr
            && let Some(dev) = self.device.as_mut()
        {
            dev.get_ioint_info(&mut *self.record, &mut self.common, IoIntrCmd::Delete);
            let posts = dev.take_device_posts();
            self.post_device_values(posts);
            self.record.set_io_intr_scan(false);
        }
        if let Some(dev) = self.device.as_mut() {
            let _ = dev.del_record();
        }
        drop(self.del_record());
        true
    }

    /// C's `PN_LINK`: a SoftCallback `add_record` that allocated its devPvt
    /// re-typed INP and kept only the severity bits
    /// (`devAiSoftCallback.c:114-115` and the input twins).
    pub(crate) fn inp_is_pn_link(&self) -> bool {
        self.dpvt.callback().is_some()
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

    /// Resolve a field for EPICS `$` long-string (character-array) access:
    /// the text C's `DBR_CHAR` view of it holds, or `None` where `$` is
    /// `S_dbLib_fieldNotFound`. Eligibility is
    /// [`char_view`](crate::server::database::char_view::char_view)'s, the
    /// one owner of the `$` rule, so a long-string record's `VAL` (a CHAR
    /// carrier here, `DBF_STRING` to C) takes the modifier like any string.
    pub fn resolve_string_view_field(&self, name: &str) -> Option<EpicsValue> {
        let mut text = crate::server::database::char_view::char_view(self, name)?.bytes();
        text.truncate(text.iter().position(|&c| c == 0).unwrap_or(text.len()));
        Some(EpicsValue::String(crate::types::PvString::from_bytes(text)))
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

    /// C `ellCount(&precord->rdes->devList) > 0`: the loaded `.dbd` set
    /// declares a `device()` for this type, statically or contributed.
    /// C `pfldDes->isDevLink` on a record type with a device menu
    /// (`dbAccess.c:1115`): a put to it goes through the device-link gates.
    pub(crate) fn is_device_link(&self, field: &str) -> bool {
        (field.eq_ignore_ascii_case("INP") || field.eq_ignore_ascii_case("OUT"))
            && self.has_device_list()
    }

    fn has_device_list(&self) -> bool {
        let record_type = self.record.record_type();
        super::dbd_generated::device_menu(record_type).is_some()
            || super::has_contributed_device_menu(record_type)
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
        if !self.has_device_list() {
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
        text: &[u8],
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
    ///   rset ([`Record::enum_slots`]).
    ///
    /// `None` when the field has neither — C answers `S_db_noRSET`, an error;
    /// the port renders empty.
    ///
    /// Each class brings its own out-of-range rule with it (see
    /// [`EnumOverflow`](crate::server::snapshot::EnumOverflow)); the index is
    /// rendered as a number for a `DBF_MENU` and ONLY for a `DBF_MENU`.
    pub(crate) fn enum_string_form_for(&self, field: &str) -> Option<EnumStringForm> {
        enum_string_form_with(self.record.as_ref(), field, || self.device_choices())
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
    /// [`LinkReadAs::String`].
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
        self.source_render_of(field, &value).dbr_string(value)
    }

    /// What [`SourceRender`] needs of this record to render `value`, just read
    /// from `field`, as `DBR_STRING`: taken under the read's hold, so the
    /// render after it never goes back for the record.
    ///
    /// C `dbGetLink` converts at the source, through `dbConvert.c`'s
    /// `[field_type][DBR_STRING]` table, inside `dbGet`: a float field renders
    /// at its own precision (`getDoubleString`), a `DBF_ENUM`/`DBF_MENU` index
    /// as its state label (`getEnumString`/`getMenuString`), and a long-string
    /// field is `DBF_STRING` to C's `cvt_dbaddr`, not an array. The label goes
    /// through [`Self::enum_string_form_for`], the owner the CA/PVA encoders
    /// use, so a link read and a `caget -t` of one field cannot disagree.
    pub(crate) fn source_render_of(&self, field: &str, value: &EpicsValue) -> SourceRender {
        // Only an array value reads the flag, so a scalar pays no lookup.
        let long_string = value.is_array() && self.record.field_is_long_string(field);
        let mut render = SourceRender {
            long_string,
            precision: None,
            label: None,
        };
        let head = if value.is_array() && !long_string {
            value.first_element()
        } else {
            None
        };
        match head.as_ref().unwrap_or(value) {
            EpicsValue::Double(_) | EpicsValue::Float(_) => {
                render.precision = Some(self.dbr_string_precision(field, value));
            }
            // A `DBF_ENUM` index, and a `DBF_MENU` index (stored as a short),
            // render through the field's string source. A short field that is
            // neither has no source and stays the plain number C converts it to.
            &EpicsValue::Enum(idx) => render.label = self.label_of(field, idx),
            // A menu index is an `epicsEnum16`: a negative short reads as its
            // unsigned bits, which `cvt_menu_st` prints with `%u`.
            &EpicsValue::Short(v) => render.label = self.label_of(field, v as u16),
            _ => {}
        }
        render
    }

    /// [`Self::enum_string_form_for`]'s label for `idx`; a menu's or a `VAL`
    /// state's is cloned from where it lives, without building the form under
    /// the read's hold.
    fn label_of(&self, field: &str, idx: u16) -> Option<PvString> {
        let desc = field_desc_of(self.record.as_ref(), field);
        if is_device(desc) {
            // `cvt_device_st`: a type with no device menu reads ""
            // (`dbFastLinkConv.c:1615-1618`).
            return Some(
                self.device_choices()
                    .map_or_else(PvString::default, |c| EnumStringForm::device(c).render(idx)),
            );
        }
        if let Some(choices) = menu_choices_with(self.record.as_ref(), field, desc) {
            return Some(match choices.get(idx as usize) {
                Some(choice) => PvString::from(*choice),
                None => PvString::from(idx.to_string().as_str()),
            });
        }
        if field.eq_ignore_ascii_case("VAL") {
            return self
                .record
                .enum_slots()
                .map(|slots| PvString::from(slots.label(idx)));
        }
        self.enum_string_form_for(field)
            .map(|form| form.render(idx))
    }

    /// C `getDoubleString`'s precision for `field` holding `value`: seeded 6
    /// and overwritten by the rset's `get_precision` — the answer the
    /// DBR_STRING encoder reads off [`Snapshot::precision`].
    ///
    /// [`Snapshot::precision`]: crate::server::snapshot::Snapshot::precision
    fn dbr_string_precision(&self, field: &str, value: &EpicsValue) -> u16 {
        let support = self
            .record
            .property_support()
            .narrowed_to_field(value.db_field_type(), false);
        if !support.precision {
            return 6;
        }
        self.record
            .field_metadata_override(field)
            .and_then(|ov| ov.precision)
            .unwrap_or_else(|| self.metadata_limit("PREC") as i16) as u16
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
    /// ([`Self::snapshot_for_field`]) and the MONITOR path (a post's log,
    /// `FieldSubscriber::declared_view`), so all three announce and serve the same
    /// type.
    ///
    /// The projection is [`EpicsValue::convert_to`], the one value-coercion
    /// owner — the same routine `dbGet` converts through. Never re-derive a
    /// conversion here: C picks its routine from BOTH the source and the
    /// destination type, and only `convert_to` knows that table.
    ///
    /// Idempotent: a value already of its declared type is short-circuited by
    /// `into_type`, and re-projecting a projected value is a no-op. That is
    /// what lets the CA path derive the native type from the value it is about
    /// to serve.
    ///
    /// A long-string field ([`FieldDesc::long_string`]) stays the `LEN`
    /// elements it holds: its plain `DBF_STRING` address and its `$`
    /// `DBR_CHAR` address read the same buffer differently, so each channel
    /// projects for itself, and a `String` here would already have lost `LEN`.
    pub fn project_to_declared_type(&self, field: &str, value: EpicsValue) -> EpicsValue {
        match self.declared_projection(field, &value) {
            Some(declared) => value.into_type(declared),
            None => value,
        }
    }

    /// The type [`Self::project_to_declared_type`] converts `value` to, or
    /// `None` when it already serves as it is.
    fn declared_projection(&self, field: &str, value: &EpicsValue) -> Option<DbFieldType> {
        declared_projection_of(self.declared_field_type(field), value, || {
            self.record.field_is_long_string(field)
        })
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
        self.record.link_backed_metadata_field(&upper_field(field))
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

    /// The one finishing pipeline behind every `Snapshot` a read serves
    /// ([`Self::snapshot_for_field`] and the doors around it); a post carries
    /// only a log, and its reader finishes the delivery read here. Every step that shapes a served snapshot — alarm/utag
    /// carry, the metadata cache, per-field routing and RSET overrides, menu
    /// enums, property support — runs here, so the two paths cannot drift
    /// apart.
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
        // straight into the `Int32` `timeStamp.userTag`. A PVA reader's
        // `Q:time:tag` split overrides this when configured, matching
        // pvxs `if(info.nsecMask) utag = meta.time.nsec & info.nsecMask;`
        // (:246-247 — the test and its assignment); CA serves it raw.
        snap.user_tag = self.common.utag as i32;
        // Carry the record's committed alarm message (`common.amsg`) so a
        // PVA read serves `alarm.message` from the record's own amsg
        // (pvxs `iocsource.cpp:230-236` prefers `meta.amsg`) rather than a
        // string re-synthesized from the condition code. Empty for records
        // that raise no message (C's plain `recGblSetSevr` clears namsg).
        snap.alarm.amsg = String::from(&self.common.amsg);

        // Pull display/control/enums from the metadata cache (seeded at init,
        // rebuilt by every metadata-class field write).
        self.finish_metadata(field, self.cached_metadata(), backing, &mut snap);
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
    pub fn qtime_nsec_mask(&self) -> u64 {
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
        let tag = self.cold.info.get("Q:form")?;
        FORM_NAMES
            .iter()
            .position(|name| name == tag)
            .map(|i| i as i16)
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
        // DESC arm of `put_common_field`, which rebuilds without
        // posting DBE_PROPERTY (epics-base#785 / UI-106).
        snap.display
            .get_or_insert_with(Default::default)
            .description = self.common.text.desc.clone();
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
        self.metadata_field(field)
            .and_then(|v| v.to_f64())
            .unwrap_or(0.0)
    }

    /// A metadata source field, read only where the record declares it: C
    /// reads these out of record memory, so an undeclared one (`EGU` on `bo`)
    /// is absent, and asking the virtual-name fallback for it is a by-name
    /// compare on every metadata rebuild.
    fn metadata_field(&self, field: &str) -> Option<EpicsValue> {
        let desc = self.field_desc(field)?;
        self.resolve_field_upper_at(field, Some(desc))
    }

    /// `EGU`, the source every ported C `get_units` copies from. Empty for a
    /// record type whose `.dbd` declares no `EGU` — `seq` and `histogram` have
    /// none, and C writes nothing into the `dbAccess.c:378` seed for either.
    fn metadata_units(&self) -> crate::types::PvString {
        match self.metadata_field("EGU") {
            Some(EpicsValue::String(s)) => s,
            _ => Default::default(),
        }
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
            snap.enums = Some(match self.record.enum_slots().map(EnumSlots::form) {
                Some(form) => super::super::snapshot::EnumInfo::with_string_form(strings, form),
                None => super::super::snapshot::EnumInfo::new(strings),
            });
        }
    }

    /// Get a common field value.
    pub fn get_common_field(&self, name: &str) -> Option<EpicsValue> {
        self.common_field(CommonField::by_name(name)?)
    }

    /// The value of the field [`Self::get_common_field`] answers as `field`.
    fn common_field(&self, field: CommonField) -> Option<EpicsValue> {
        match field {
            CommonField::Sevr => Some(EpicsValue::Short(self.common.sevr as i16)),
            CommonField::Stat => Some(EpicsValue::Short(self.common.stat as i16)),
            CommonField::Nsev => Some(EpicsValue::Short(self.common.nsev as i16)),
            CommonField::Nsta => Some(EpicsValue::Short(self.common.nsta as i16)),
            // epics-base PR #568 / #566 — alarm message string.
            CommonField::Amsg => Some(EpicsValue::String(String::from(&self.common.amsg).into())),
            CommonField::Namsg => Some(EpicsValue::String(String::from(&self.common.namsg).into())),
            CommonField::Acks => Some(EpicsValue::Short(self.common.acks as i16)),
            // `ACKT` and `PINI` are `DBF_MENU` (`menuYesNo` /`menuPini`,
            // `dbCommon.dbd.pod:335,169`), not `DBF_UCHAR`: they carry a menu
            // index, which `promote_menu_value` lifts to `DBR_ENUM` with the
            // menu's choice strings. Storing them as `Short` is what makes
            // them eligible for that promotion — see `promote_menu_value`.
            CommonField::Ackt => Some(EpicsValue::Short(if self.common.ackt { 1 } else { 0 })),
            // DBF_UCHAR: served as UChar (declared type) so the raw put byte
            // round-trips to the wire — see the DISP/TPRO comment above.
            CommonField::Udf => Some(EpicsValue::UChar(self.common.udf)),
            CommonField::Udfs => Some(EpicsValue::Short(self.common.udfs)),
            CommonField::Scan => Some(EpicsValue::Enum(self.common.scan.to_u16())),
            CommonField::Sscn => Some(EpicsValue::Enum(self.common.sscn.to_u16())),
            // `OLDSIMM` is `DBF_MENU`/`menu(menuSimm)`, stored as the menu index
            // and promoted to `DBR_ENUM` with the NO/YES/RAW labels by
            // `promote_menu_value` (shared registry — the saved copy is ALWAYS
            // menuSimm, unlike the live SIMM). Written only by the simulation
            // owner (`rec_gbl_save_simm`); `special(SPC_NOMOD)` for clients.
            CommonField::Oldsimm => Some(EpicsValue::Short(self.common.oldsimm)),
            CommonField::Pini => Some(EpicsValue::Short(self.common.pini)),
            // DISP/TPRO/RPRO/UDF are `DBF_UCHAR` in `dbCommon.dbd`. Serve them
            // as `UChar` — their DECLARED type — so `project_to_declared_type`
            // is identity and the raw put byte reaches the wire untouched (C
            // stores the byte and `caget` renders `DBR_CHAR` signed: 255 → -1).
            // Serving `Char` here instead routed the value through the lossy
            // `Char → UChar` projection (signed −1 clamped to 0), so a
            // `caput DISP 255` read back as 0 rather than C's -1. `BKPT` is
            // `DBF_NOACCESS`: no `FieldDesc`, no projection, served `Char`.
            CommonField::Tpro => Some(EpicsValue::UChar(self.common.tpro)),
            CommonField::Bkpt => Some(EpicsValue::Char(self.common.bkpt.get())),
            CommonField::Flnk => Some(EpicsValue::String(self.common.text.flnk.clone().into())),
            // A record type whose C `.dbd` has no INP has no `.INP` channel
            // either — C's dbChannel resolution is the dbd, so `dbgf HI.INP` on
            // a histogram answers "PV 'HI.INP' not found". The port keeps INP on
            // `CommonFields` for every record, so `declares_inp_link()` is what
            // stands in for the dbd, and it must gate the read side as well as
            // the write side (`put_common_field`) — otherwise the field is
            // unloadable and unwritable yet still resolves as a channel.
            CommonField::Inp if self.record.declares_inp_link() => {
                Some(EpicsValue::String(self.common.text.inp.clone().into()))
            }
            CommonField::Out => Some(EpicsValue::String(self.common.text.out.clone().into())),
            // C's DTYP is `DBF_DEVICE`: an epicsEnum16 index into the record
            // type's device menu, NOT the name. The name is what this port
            // stores and dispatches on; the index is what the wire carries.
            CommonField::Dtyp => Some(EpicsValue::Enum(self.dtyp_index())),
            CommonField::Tse => Some(EpicsValue::Short(self.common.tse())),
            CommonField::Tsel => Some(EpicsValue::String(self.common.text.tsel.clone().into())),
            // C `UTAG` is DBF_UINT64 — exposed natively as the unsigned
            // 64-bit value variant so values above i64::MAX round-trip.
            CommonField::Utag => Some(EpicsValue::UInt64(self.common.utag)),
            CommonField::Asg => Some(EpicsValue::String(self.common.text.asg.clone().into())),
            CommonField::Asl => Some(EpicsValue::Char(self.common.asl)),
            CommonField::Desc => Some(EpicsValue::String(self.common.text.desc.clone())),
            CommonField::Phas => Some(EpicsValue::Short(self.common.phas)),
            CommonField::Evnt => Some(EpicsValue::String(self.common.text.evnt.clone())),
            CommonField::Prio => Some(EpicsValue::Short(self.common.prio)),
            CommonField::Disv => Some(EpicsValue::Short(self.common.disv)),
            CommonField::Disa => Some(EpicsValue::Short(self.common.disa)),
            CommonField::Sdis => Some(EpicsValue::String(self.common.text.sdis.clone().into())),
            CommonField::Diss => Some(EpicsValue::Short(self.common.diss)),
            CommonField::Hyst => Some(EpicsValue::Double(self.common.hyst)),
            CommonField::Lcnt => Some(EpicsValue::Short(self.common.lcnt)),
            CommonField::Disp => Some(EpicsValue::UChar(self.common.disp)),
            CommonField::Putf => Some(EpicsValue::Char(if self.common.putf { 1 } else { 0 })),
            CommonField::Rpro => Some(EpicsValue::UChar(self.common.rpro)),
            CommonField::Pact => Some(EpicsValue::Char(
                (self.is_processing()
                    || (self.cold.cell.upgrade())
                        .is_some_and(|cell| cell.chain_pact.load(Ordering::Relaxed)))
                    as u8,
            )),
            // C `dbCommon.dbd`: `field(PROC,DBF_UCHAR)` — the raw put byte is
            // retained in `prec->proc` and served back SIGNED as `DBR_CHAR`
            // (`caput PROC 255` → `caget` = -1), exactly like DISP/RPRO. The
            // `pp(TRUE)` force-process is orthogonal: writing PROC still
            // reprocesses the record (put-path intercept), but the byte sticks.
            CommonField::Proc => Some(EpicsValue::UChar(self.common.proc_field)),
            // Analog alarm fields
            CommonField::Hihi => self
                .common
                .analog_alarm
                .as_ref()
                .map(|a| a.limit(AlarmLimitField::Hihi).to_epics_value()),
            CommonField::High => self
                .common
                .analog_alarm
                .as_ref()
                .map(|a| a.limit(AlarmLimitField::High).to_epics_value()),
            CommonField::Low => self
                .common
                .analog_alarm
                .as_ref()
                .map(|a| a.limit(AlarmLimitField::Low).to_epics_value()),
            CommonField::Lolo => self
                .common
                .analog_alarm
                .as_ref()
                .map(|a| a.limit(AlarmLimitField::Lolo).to_epics_value()),
            CommonField::Hhsv => self
                .common
                .analog_alarm
                .as_ref()
                .map(|a| EpicsValue::Short(a.hhsv)),
            CommonField::Hsv => self
                .common
                .analog_alarm
                .as_ref()
                .map(|a| EpicsValue::Short(a.hsv)),
            CommonField::Lsv => self
                .common
                .analog_alarm
                .as_ref()
                .map(|a| EpicsValue::Short(a.lsv)),
            CommonField::Llsv => self
                .common
                .analog_alarm
                .as_ref()
                .map(|a| EpicsValue::Short(a.llsv)),
            // swait OUTN is aliased to common.text.out
            CommonField::Outn => {
                if self.record.record_type() == "swait" {
                    Some(EpicsValue::String(self.common.text.out.clone().into()))
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
    /// This separates the two meanings a link field carries. `common.text.inp` /
    /// `common.text.out` is the link *text* — always the value the `.db` file
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
        use crate::server::device_support::IoIntrCmd;
        let old_scan = self.common.scan;
        if old_scan == new_scan {
            return CommonFieldPutResult::NoChange;
        }
        // C `scanDelete`/`scanAdd` call the record's device support
        // `get_ioint_info(1)` / `get_ioint_info(0)`. Only a change of I/O Intr
        // *membership* reaches those; a Passive→"1 second" move calls neither.
        // `scanDelete` asks with the old SCAN still in place, `scanAdd` with
        // the new one, and a refusal there leaves the record Passive.
        // Before `scanInit` the move is only stored: `buildScanLists` joins
        // the record by its SCAN then. C's `recGblInitSimm` swap asks here as
        // well, so `get_ioint_info(0)` runs twice and the record is filed on
        // the list twice (recGbl.c:430-435, dbScan.c:1073): not copied.
        let listed = self.scan_lists_built();
        let was_io_intr = old_scan == ScanType::IoIntr;
        let mut is_io_intr = new_scan == ScanType::IoIntr;
        if listed && was_io_intr && !is_io_intr {
            if let Some(dev) = self.device.as_mut() {
                dev.get_ioint_info(&mut *self.record, &mut self.common, IoIntrCmd::Delete);
                let posts = dev.take_device_posts();
                self.post_device_values(posts);
            }
        }
        self.common.scan = new_scan;
        if listed && is_io_intr && !was_io_intr {
            if let Err(message) = self.scan_add_io_intr() {
                if let Some(message) = message {
                    // C passes `-1`, whose status slot `recGblRecordError`
                    // leaves empty (recGbl.c:65-70).
                    crate::server::recgbl::rec_gbl_record_error(
                        "",
                        &self.name.to_str_lossy(),
                        message,
                    );
                }
                self.common.scan = ScanType::Passive;
                is_io_intr = false;
            }
        }
        if was_io_intr != is_io_intr && !self.record.set_io_intr_scan(is_io_intr) {
            // `scanAdd`'s `precord->scan = menuScanPassive` (dbScan.c:286-288).
            self.common.scan = ScanType::Passive;
        }
        let new_scan = self.common.scan;
        if old_scan == new_scan {
            return CommonFieldPutResult::NoChange;
        }
        CommonFieldPutResult::ScanChanged {
            old_scan,
            new_scan,
            phas: self.common.phas,
        }
    }

    /// Whether the record's database has built its scan lists. A record in
    /// no database is still being loaded, where C's SCAN is only stored.
    fn scan_lists_built(&self) -> bool {
        self.cold
            .db
            .upgrade()
            .is_some_and(|db| db.scan_control().lists_built())
    }

    /// C `scanAdd`'s I/O Intr arm (dbScan.c:268-293), for iocInit, a SCAN
    /// put and `restoreScan` alike: the dset row's hook, `get_ioint_info(0)`,
    /// then the list the device hands back, which the pump serves. `Err` is
    /// C's `menuScanPassive` exit with its `recGblRecordError` text, `None`
    /// for a hook that declined, which C demotes without a line. The
    /// trailing space of the no-DSET text is C's.
    pub(crate) fn scan_add_io_intr(&mut self) -> Result<(), Option<&'static str>> {
        use crate::server::device_support::IoIntrCmd;
        const NO_DSET: &str = "scanAdd: I/O Intr not valid (no DSET) ";
        const NO_HOOK: &str = "scanAdd: I/O Intr not valid (no get_ioint_info)";
        const NO_LIST: &str = "scanAdd: I/O Intr not valid";
        let row = super::dev_sup_of(self.record.record_type(), self.common.dtyp.as_str());
        if row.is_some_and(|row| !row.has_get_ioint_info()) {
            return Err(Some(NO_HOOK));
        }
        // A soft device is served by the framework, so no device object
        // says nothing about C's `precord->dset`; its devSup row does. A row
        // with the hook but no object to answer it is `:290-293`'s NULL list.
        let Some(dev) = self.device.as_mut() else {
            return Err(Some(if row.is_none() { NO_DSET } else { NO_LIST }));
        };
        let added = dev.get_ioint_info(&mut *self.record, &mut self.common, IoIntrCmd::Add);
        let posts = dev.take_device_posts();
        self.post_device_values(posts);
        if !added {
            return Err(None);
        }
        if !self.wire_io_intr_pump() {
            return Err(Some(NO_LIST));
        }
        Ok(())
    }

    /// Start the task that turns the bound device's wakes into processing,
    /// the one place that starts it. A device hands its receiver out once
    /// per object, so a fresh one is a newly bound dset and replaces the old
    /// pump; leaving the list stops nothing, because the pump only processes
    /// a record that is on it (C `scanIoRequest` walks the list's members).
    /// Whether the record now has a live pump.
    pub(crate) fn wire_io_intr_pump(&mut self) -> bool {
        let pumped = |p: &Option<crate::runtime::background::AbortHandle>| {
            p.as_ref().is_some_and(|p| !p.is_finished())
        };
        let Some(dev) = self.device.as_mut() else {
            return pumped(&self.cold.io_intr_pump);
        };
        let Some(rx) = dev.io_intr_receiver() else {
            return pumped(&self.cold.io_intr_pump);
        };
        let (Some(db), Some(cell)) = (self.cold.db.upgrade(), self.cold.cell.upgrade()) else {
            return false;
        };
        let pump = crate::server::ioc_app::spawn_io_intr_pump(
            db,
            self.name.clone(),
            cell,
            rx,
            self.common.callback_priority(),
            dev.io_intr_processes_directly(),
            dev.io_intr_scan_independent(),
        );
        if let Some(old) = self.cold.io_intr_pump.replace(pump.abort_handle()) {
            old.abort();
        }
        true
    }

    /// What iocInit gives a dset before its record joins any list, for the
    /// one a device-link put bound: the database's gates, its PROPERTY
    /// drain, and the pump of a device that processes whatever the SCAN. An
    /// I/O Intr record's pump follows from `restoreScan`'s join.
    fn wire_bound_device(&mut self) {
        if let (Some(dev), Some(db)) = (self.device.as_mut(), self.cold.db.upgrade()) {
            dev.bind_interrupt_gate(db.scan_control().interrupt_gate());
            dev.bind_dev_sup_gate(db.scan_control().dev_sup_gate());
        }
        self.wire_property_posts();
        if self
            .device
            .as_ref()
            .is_some_and(|d| d.io_intr_scan_independent())
        {
            self.wire_io_intr_pump();
        }
    }

    /// Start the task that posts the bound device's PROPERTY changes, the
    /// one place that starts it: C's `registerInterruptUser(callbackEnum)`
    /// belongs to whichever dset object is bound, iocInit's or a device-link
    /// put's, and runs whatever the SCAN. A fresh receiver is a newly bound
    /// dset and replaces the old drain. Whether the record now has a live one.
    pub(crate) fn wire_property_posts(&mut self) -> bool {
        let drained = |p: &Option<crate::runtime::background::AbortHandle>| {
            p.as_ref().is_some_and(|p| !p.is_finished())
        };
        let Some(dev) = self.device.as_mut() else {
            return drained(&self.cold.property_drain);
        };
        let Some(mut rx) = dev.property_post_receiver() else {
            return drained(&self.cold.property_drain);
        };
        let Some(db) = self.cold.db.upgrade() else {
            return false;
        };
        dev.bind_interrupt_gate(db.scan_control().interrupt_gate());
        let rec_name = self.name.clone();
        let drain =
            crate::runtime::task::spawn_background(self.common.callback_priority(), async move {
                // Each message is the setEnums field block plus the one field
                // C posts on; DBE_PROPERTY on that field is what makes clients
                // re-read the choices.
                while let Some(post) = rx.recv().await {
                    let _ = db.post_property(&rec_name, post);
                }
            });
        if let Some(old) = self.cold.property_drain.replace(drain.abort_handle()) {
            old.abort();
        }
        true
    }

    /// C `scanAdd`'s failure exits, `precord->scan = menuScanPassive`
    /// (dbScan.c:272-293): the record never joined the I/O Intr list, so no
    /// `scanDelete` and no `get_ioint_info(1)` runs.
    pub fn demote_io_intr_scan(&mut self) -> CommonFieldPutResult {
        if self.common.scan != ScanType::IoIntr {
            return CommonFieldPutResult::NoChange;
        }
        self.common.scan = ScanType::Passive;
        self.record.set_io_intr_scan(false);
        CommonFieldPutResult::ScanChanged {
            old_scan: ScanType::IoIntr,
            new_scan: ScanType::Passive,
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
        // C `recGblSaveSimm`: `if (*psscn == USHRT_MAX) return;` — the literal
        // sentinel, not "any index outside the menu". Asked before the
        // helpers question: both answers return, and SSCN unset is the cell
        // read, where the helpers question is a virtual call and a name match.
        if self.common.sscn.is_unset() || !self.record.uses_recgbl_simm_helpers() {
            return;
        }
        if let Some(simm) = self.record.simm_value() {
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
        let Some(sim_scan) = self.common.sscn.scan() else {
            // `*psscn == USHRT_MAX` — SSCN unset, no swap. An SSCN that is
            // merely ILLEGAL still swaps: C assigns it into SCAN and `scanAdd`
            // then declines to scan the record. Asked before the helpers
            // question, which is a virtual call and a name match.
            return CommonFieldPutResult::NoChange;
        };
        if !self.record.uses_recgbl_simm_helpers() {
            return CommonFieldPutResult::NoChange;
        }
        let Some(simm) = self.record.simm_value() else {
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
    pub fn put_ackt(&mut self, value: u16) {
        self.put_ackt_in(&mut PostBurst::new(), value);
    }

    /// [`Self::put_ackt`] posting into the put body's `burst`.
    pub fn put_ackt_in(&mut self, burst: &mut PostBurst, value: u16) {
        let new_ackt = value != 0;
        if new_ackt == self.common.ackt {
            return;
        }
        use crate::server::recgbl::EventMask;
        let ack_mask = EventMask::VALUE | EventMask::ALARM;
        self.common.ackt = new_ackt;
        self.notify_declared(Some(burst), common_desc::ackt(), ack_mask);
        // C `:1294-1297`: turning transient acknowledgement off lowers a
        // sticky ACKS down to the current SEVR — an alarm that has already
        // cleared must not keep a higher unacknowledged severity.
        if !new_ackt && self.common.acks > self.common.sevr {
            self.common.acks = self.common.sevr;
            self.notify_declared(Some(burst), common_desc::acks(), ack_mask);
        }
        self.notify_record_alarm_in(burst);
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
    pub fn put_acks(&mut self, value: u16) {
        self.put_acks_in(&mut PostBurst::new(), value);
    }

    /// [`Self::put_acks`] posting into the put body's `burst`.
    pub fn put_acks_in(&mut self, burst: &mut PostBurst, value: u16) {
        let sev = AlarmSeverity::from_u16(value);
        if sev < self.common.acks {
            return;
        }
        use crate::server::recgbl::EventMask;
        self.common.acks = AlarmSeverity::NoAlarm;
        self.notify_declared(
            Some(burst),
            common_desc::acks(),
            EventMask::VALUE | EventMask::ALARM,
        );
        self.notify_record_alarm_in(burst);
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
        let mut link = PreparedCommonLink::for_put(name, &value);
        if let Some(link) = &mut link {
            self.check_link_assignment(link.field, link.text.as_bytes(), bound)?;
            // The database's put bodies run the gate under their own hold.
            if matches!(bound, MenuBound::DbPut) {
                self.gate_device_link_put(link)?;
            }
        }
        let put = self.put_common_field_with(name, value, bound, &mut link);
        let scan = match link.as_mut() {
            Some(link) => self.take_link_scan(link),
            None => CommonFieldPutResult::NoChange,
        };
        match put {
            Ok(CommonFieldPutResult::NoChange) => Ok(scan),
            put => put,
        }
    }

    /// The common-field writer for a runtime put whose `dbCommon` link text
    /// was parsed and held to `dbCanSetLink` before the hold. The old text
    /// and parse come back in `link`, to be dropped after the hold.
    pub(crate) fn put_common_field_prepared(
        &mut self,
        name: &str,
        value: EpicsValue,
        link: &mut Option<PreparedCommonLink>,
    ) -> CaResult<CommonFieldPutResult> {
        self.put_common_field_with(name, value, MenuBound::DbPut, link)
    }

    /// [`Self::put_common_field_prepared`] for the `.db` loader's bound.
    pub(crate) fn put_common_field_db_load_prepared(
        &mut self,
        name: &str,
        value: EpicsValue,
        link: &mut Option<PreparedCommonLink>,
    ) -> CaResult<CommonFieldPutResult> {
        self.put_common_field_with(name, value, MenuBound::DbLoad, link)
    }

    /// C `dbPutFieldLink`'s `isDevLink` arm (`dbAccess.c:1127-1131`,
    /// `:1183-1201`): a runtime INP/OUT put re-takes `dset` from the CURRENT
    /// DTYP once `dbPutSpecial(paddr, 1)` has passed, so a DTYP put made after
    /// init takes effect only once the device link is written, and the device
    /// private starts over. The loader's puts are bound at init instead
    /// (`dbInitRecord`).
    ///
    /// The new object was built and asked `add_record` before the hold; here
    /// it only moves onto the record, and the one it replaces leaves with
    /// `link` to be dropped after the hold. A failed `special` or a refused
    /// `add_record` leaves the record with no dset and PACT set
    /// (`:1185-1186`, `:1198-1199`), so every process entry takes the
    /// active-record refusal (`count_refused_active_entry`) until a
    /// device-link put binds again. Otherwise `dpvt = NULL` (`:1192`) and
    /// `pact = FALSE` (`:1194`) whoever set PACT: the put owns it here, and a
    /// pending continuation finds it released and runs a fresh cycle, as C's
    /// delayed `process()` does.
    ///
    /// `special` is the after-store pass's answer, carried through on success.
    fn rebind_device_link<R>(
        &mut self,
        bound: MenuBound,
        link: &mut PreparedCommonLink,
        special: CaResult<R>,
    ) -> CaResult<R> {
        if !matches!(bound, MenuBound::DbPut) || !self.has_device_list() {
            return special;
        }
        let Some(prepared) = link.device.as_mut() else {
            return special;
        };
        self.common.dtyp.rebind(self.record.record_type());
        let reaches_add_record = special.is_ok();
        // What is not installed stays in `prepared` and is freed with it,
        // after the hold.
        let rebound = match special {
            Err(e) => Err(e),
            Ok(passed) => match std::mem::replace(&mut prepared.dset, NewDset::Keep) {
                NewDset::Keep => Ok(passed),
                NewDset::SoftRefused(refusal) => {
                    let e = CaError::BadLink(crate::error::LinkRefusal::add_record(refusal.dset()));
                    link.add_record_refusal = Some(refusal);
                    Err(e)
                }
                NewDset::Absent => {
                    prepared.retired = self.device.take();
                    Ok(passed)
                }
                NewDset::Bound(mut dev) => {
                    dev.set_record_info(&self.name.to_str_lossy(), self.common.scan);
                    dev.apply_record_info(self.info());
                    match dev.add_record(&mut *self.record) {
                        Ok(()) => {
                            prepared.retired = self.device.replace(dev);
                            Ok(passed)
                        }
                        Err(refusal) => {
                            prepared.dset = NewDset::Bound(dev);
                            Err(CaError::BadLink(refusal))
                        }
                    }
                }
            },
        };
        prepared.retired_dpvt = self.del_record();
        // C `dpvt = NULL` (`:1192`) precedes `add_record`, so a refused one
        // leaves it NULL too; `del_record` took any devPvt that owns memory.
        if reaches_add_record {
            self.dpvt = std::mem::take(&mut prepared.dpvt);
        }
        let passed = match rebound {
            Ok(passed) => passed,
            Err(e) => {
                self.refuse_device_link_put(prepared, reaches_add_record);
                return Err(e);
            }
        };
        // C restarts a queued put-notify at a cycle's completion, not at this
        // store, so the token has no tail to reach.
        let _ = self.leave_pact();
        self.wire_bound_device();
        self.finish_link_scan(prepared, true);
        Ok(passed)
    }

    /// [`Self::rebind_device_link`] for a `dbPutField` of a device link the
    /// record stores itself (epid INP, scaler OUT), after its
    /// `special(_, true)`; a `dbCommon` INP/OUT rebinds in
    /// [`Self::put_common_field_prepared`].
    pub(crate) fn rebind_declared_device_link<R>(
        &mut self,
        link: &mut Option<PreparedCommonLink>,
        special: CaResult<R>,
    ) -> CaResult<R> {
        match link {
            Some(link) => self.rebind_device_link(MenuBound::DbPut, link, special),
            None => special,
        }
    }

    /// C `dbPutFieldLink`'s refusal (`dbAccess.c:1149-1153`) of a runtime
    /// device-link put whose DTYP names a device with no extension table
    /// (`dsxt`): `S_db_noSupport`, silent, before the link moves, so the link
    /// and the bound dset stay. Run ahead of the store whichever writer owns
    /// the field: scaler keeps OUT itself. The devSup row answers it
    /// ([`DevSup::has_dsxt`](crate::server::record::DevSup::has_dsxt)), whichever crate registered it. A DTYP with no
    /// row, which only a dynamic device support can bind and C cannot
    /// express, is not judged.
    ///
    /// It is also where a put whose DTYP moved after its dset was prepared
    /// is refused, before anything moves: the put entry has already rebuilt
    /// once ([`PreparedCommonLink::device_dtyp_moved`]). Past both, the bound
    /// object's `del_record` may still keep the link
    /// ([`crate::server::device_support::DeviceSupport::del_record`]); C's `restoreScan` then leaves the
    /// record as it found it.
    ///
    /// Between the two, an I/O Intr record leaves its list through
    /// [`Self::set_scan`] while the old dset is still bound
    /// (`dbAccess.c:1155-1158`); every exit of the put then reaches
    /// [`Self::finish_link_scan`].
    pub(crate) fn gate_device_link_put(&mut self, link: &mut PreparedCommonLink) -> CaResult<()> {
        if link.device_dtyp_moved(self) {
            return Err(CaError::NoSupport);
        }
        // The new DTYP's dset and the bound one must both have a `dsxt`
        // (`dbAccess.c:1149-1152`).
        if self.common.dtyp.bound_dsxt() == crate::server::device_support::BoundDsxt::Lacks
            || super::dev_sup_of(self.record.record_type(), self.common.dtyp.as_str())
                .is_some_and(|row| !row.has_dsxt())
        {
            return Err(CaError::NoSupport);
        }
        let Some(prepared) = link.device.as_mut() else {
            return match self.device.as_mut() {
                Some(dev) => dev.del_record().map_err(CaError::BadLink),
                None => Ok(()),
            };
        };
        if self.common.scan == ScanType::IoIntr {
            let _ = self.set_scan(ScanType::Passive);
            prepared.left_io_intr = true;
        }
        let deleted = self.device.as_mut().map_or(Ok(()), |dev| dev.del_record());
        if let Err(refusal) = deleted {
            // `goto restoreScan`: the old dset goes back on the list.
            self.finish_link_scan(prepared, true);
            return Err(CaError::BadLink(refusal));
        }
        Ok(())
    }

    /// C `dbPutFieldLink`'s `restoreScan` and `postScanEvent` exits
    /// (`dbAccess.c:1218-1225`) for a put whose `scanDelete` ran. `restore`
    /// is `scanAdd` with the dset now bound, whose refusal leaves the record
    /// Passive; a record that ends off I/O Intr posts SCAN and leaves the
    /// move for [`Self::take_link_scan`].
    fn finish_link_scan(&mut self, prepared: &mut link_cache::PreparedDevice, restore: bool) {
        if !std::mem::take(&mut prepared.left_io_intr) {
            return;
        }
        if restore {
            let _ = self.set_scan(ScanType::IoIntr);
        }
        if self.common.scan != ScanType::IoIntr {
            prepared.scan_moved = CommonFieldPutResult::ScanChanged {
                old_scan: ScanType::IoIntr,
                new_scan: self.common.scan,
                phas: self.common.phas,
            };
            use crate::server::recgbl::EventMask;
            self.notify_field("SCAN", EventMask::VALUE | EventMask::LOG);
        }
    }

    /// The scan-list move a device-link put leaves, for the caller to file
    /// once the hold is down. An exit no arm settled is a failure's
    /// `postScanEvent`: the record stays Passive.
    pub(crate) fn take_link_scan(&mut self, link: &mut PreparedCommonLink) -> CommonFieldPutResult {
        let Some(prepared) = link.device.as_mut() else {
            return CommonFieldPutResult::NoChange;
        };
        self.finish_link_scan(prepared, false);
        std::mem::replace(&mut prepared.scan_moved, CommonFieldPutResult::NoChange)
    }

    /// A device-link put refused by `dbPutSpecial`'s SPC_NOMOD, which C
    /// reaches only after `dbRemoveLink` (`dbAccess.c:1167-1168`): the text
    /// stays but the link is gone, and the record leaves with no dset and
    /// PACT set (`:1183-1188`), as [`Self::rebind_device_link`]'s failure arm
    /// does.
    pub(crate) fn detach_device_link(&mut self, link: &mut PreparedCommonLink) {
        if let Some(prepared) = link.device.as_mut() {
            prepared.retired_dpvt = self.del_record();
            self.refuse_device_link_put(prepared, false);
        }
    }

    /// Every failure arm of C `dbPutFieldLink` past `dbRemoveLink`
    /// (`dbAccess.c:1183-1186`, `:1196-1199`): the record keeps no dset, which
    /// is also what says the link stays unadded, sets PACT, and `dpvt` is the
    /// caller's. A
    /// device object stands for its dpvt too, so it goes only once
    /// `add_record` was reached (`:1192`); before that it stays as
    /// `del_record` left it. An I/O Intr record stays where the put's `scanDelete` left it,
    /// Passive (`:1155-1158`, posted at `:1231-1232`); only the success arm
    /// restores it.
    fn refuse_device_link_put(
        &mut self,
        prepared: &mut link_cache::PreparedDevice,
        add_record_ran: bool,
    ) {
        if add_record_ran {
            prepared.retired = self.device.take();
        }
        self.common.dtyp.detach();
        self.enter_pact();
        self.finish_link_scan(prepared, false);
    }

    /// C `dbPutFieldLink` on a link field that is not a device link
    /// (`dbAccess.c:1168-1174`): `Some(slot)` for a field with a link slot,
    /// and a link an earlier refused put removed fails `dbLinkIsDefined`
    /// with `S_db_badHWaddr`, silently.
    pub(crate) fn link_put_slot(&self, field: &str) -> CaResult<Option<usize>> {
        let Some(i) = self.link_slots.position(field) else {
            return Ok(None);
        };
        if self.link_slots.is_removed(i) {
            return Err(CaError::BadLink(crate::error::LinkRefusal::new(
                crate::error::LinkWhy::NotDefined(self.link_slots.field(i)),
            )));
        }
        Ok(Some(i))
    }

    /// C `dbRemoveLink` on [`Self::link_put_slot`]'s slot: run before
    /// `dbPutSpecial(paddr, 0)` refuses an `SPC_NOMOD` field, and the error
    /// path does not restore it, so aSub's SUBL reads nothing afterwards.
    pub(crate) fn remove_link_slot(&mut self, i: usize) {
        self.invalidate_plan_for(self.link_slots.field(i));
        self.link_slots.remove(i);
    }

    /// An `openLink` that left `field`'s link with no lset
    /// (`pvalink_lset.cpp:70-75`): the [`ParsedLink::Removed`] C's
    /// `dbRemoveLink` leaves, in every store the record reads the link from,
    /// so its reads, puts and forward take the no-lset arms with nothing
    /// asked per cycle. A later put of the field re-parses it, as C's
    /// `dbPutFieldLink` re-adds it.
    pub(crate) fn bind_no_lset(&mut self, field: &str) {
        let Some(old) = self.stored_link_shared(field).cloned() else {
            return;
        };
        let mut removed = link_cache::removed(&old);
        if let Some(i) = self.link_slots.position(field) {
            self.link_slots.remove(i);
        }
        if let Some(slot) = self
            .record
            .multi_input_links()
            .iter()
            .position(|(f, _)| *f == field)
            && let Some(text) = link_text_of(&*self.record, field)
        {
            self.parsed_inputs[slot] = Some(ParsedInputLink::unbound(text, removed.clone()));
        }
        self.swap_link_cache(field, &mut removed);
        self.invalidate_plan_for(field);
    }

    /// The link puts whose `SPC_NOMOD` refusal waits for the hold: a device
    /// link's and [`Self::link_put_slot`]'s.
    pub(crate) fn refuses_link_put_under_hold(&self, field: &str) -> bool {
        self.is_device_link(field) || self.link_slots.position(field).is_some()
    }

    fn put_common_field_with(
        &mut self,
        name: &str,
        value: EpicsValue,
        bound: MenuBound,
        link: &mut Option<PreparedCommonLink>,
    ) -> CaResult<CommonFieldPutResult> {
        // The put entries hand the declared spelling already; only another
        // spelling allocates, under the caller's hold.
        let name = upper_field(name);
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
                && let Some(class) = crate::types::dbf_link_class(self.record.record_type(), &name)
            {
                super::check_json_link_text(&text, super::LinkFieldType::for_class(class))?;
            }
        }
        // The common fields a `CyclePlan` is resolved from. This is the funnel
        // for them, so the stamp is bumped here and not per arm.
        self.invalidate_plan_for(&name);
        match &*name {
            // `special(SPC_NOMOD)` — C `dbPutSpecial` refuses the put with
            // `S_db_noMod` (dbAccess.c:123-127). OLDSIMM is written only by the
            // simulation-mode owner (`rec_gbl_save_simm`).
            "OLDSIMM" => return Err(CaError::ReadOnlyField(name.into_owned())),
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
                    self.common.amsg.set(&s.as_str_lossy());
                }
            }
            "NAMSG" => {
                if let EpicsValue::String(s) = value {
                    self.common.namsg.set(&s.as_str_lossy());
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
                    if v != 0 {
                        crate::server::database::arm_debugging();
                    }
                    self.common.tpro = v;
                }
            }
            "BKPT" => {
                if let EpicsValue::Char(v) = value {
                    self.common.bkpt.set(v);
                }
            }
            "FLNK" => {
                if let Some(link) = link {
                    std::mem::swap(&mut self.common.text.flnk, &mut link.text);
                    self.swap_link_cache("FLNK", &mut link.parsed);
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
                if let Some(link) = link {
                    std::mem::swap(&mut self.common.text.inp, &mut link.text);
                    if !self.record_declares_field("INP") {
                        self.swap_link_cache("INP", &mut link.parsed);
                    }
                }
            }
            "OUT" => {
                if let Some(link) = link {
                    std::mem::swap(&mut self.common.text.out, &mut link.text);
                    // C `dbDbPutValue` (dbDbLink.c:386-389): an OUT
                    // link processes its target only on an explicit
                    // ` PP` token (or a `.PROC` destination). A bare
                    // OUT link is NPP — the `Out` parse downgrades the
                    // modifier-less `ProcessPassive` default.
                    if !self.record_declares_field("OUT") {
                        self.swap_link_cache("OUT", &mut link.parsed);
                    }
                    // `special(OUT, true)` (longout's OOCH hook among them)
                    // is the tail's, once the link has moved.
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
                EpicsValue::String(s) => {
                    let text = s.as_str_lossy();
                    let rtype = self.record.record_type();
                    let index = || {
                        use crate::types::c_parse::{NumericField, parse_auto_base_units_null};
                        match parse_auto_base_units_null(NumericField::UShort, &text) {
                            Some(EpicsValue::UShort(i)) => Some(usize::from(i)),
                            _ => None,
                        }
                    };
                    match super::device_menu_choices(rtype).find(|choice| *choice == text) {
                        Some(choice) => self.common.dtyp.set_menu_name(choice),
                        // `dbPutStringNum` stores an index the menu bounds
                        // check let through (`dbStaticRun.c:505-510`): C reads
                        // a choice back, or "" on a type without a menu.
                        None => {
                            match index().map(|i| (i, super::device_menu_choices(rtype).nth(i))) {
                                Some((_, Some(choice))) => self.common.dtyp.set_menu_name(choice),
                                Some(_) if super::device_menu_choices(rtype).next().is_none() => {
                                    self.common.dtyp.set_name("")
                                }
                                _ => self.common.dtyp.set_name(text),
                            }
                        }
                    }
                }
                EpicsValue::Enum(i) => {
                    match super::device_menu_choices(self.record.record_type()).nth(i as usize) {
                        Some(choice) => self.common.dtyp.set_menu_name(choice),
                        None => return Ok(CommonFieldPutResult::NoChange),
                    }
                }
                _ => return Ok(CommonFieldPutResult::NoChange),
            },
            "TSE" => {
                if let EpicsValue::Short(v) = value {
                    self.common.set_tse(v);
                }
            }
            "TSEL" => {
                if let Some(link) = link {
                    std::mem::swap(&mut self.common.text.tsel, &mut link.text);
                    self.swap_link_cache("TSEL", &mut link.parsed);
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
                    self.common.text.asg = s.as_str_lossy().into_owned();
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
                    if self.common.text.desc != s {
                        self.common.text.desc = s;
                        // DESC feeds `display.description` (a metadata-cache
                        // source) but is not property-class — C never marks
                        // it prop(YES) (epics-base#785) — so refresh the
                        // cache here at the write owner without posting
                        // DBE_PROPERTY: the pvxs behavior (fresh on the next
                        // metadata build, no event).
                        self.refresh_metadata_cache();
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
                    EpicsValue::String(s) => self.common.text.evnt = s,
                    EpicsValue::Short(v) => self.common.text.evnt = v.to_string().into(),
                    EpicsValue::Long(v) => self.common.text.evnt = v.to_string().into(),
                    EpicsValue::Enum(v) => self.common.text.evnt = v.to_string().into(),
                    EpicsValue::Double(v) => {
                        // Match C `eventNameToHandle`: a double with
                        // an integer part is treated as that integer.
                        self.common.text.evnt = (v as i64).to_string().into();
                    }
                    _ => {}
                }
            }
            "PRIO" => {
                if let EpicsValue::Short(v) = value {
                    self.set_prio(v);
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
                if let Some(link) = link {
                    std::mem::swap(&mut self.common.text.sdis, &mut link.text);
                    self.swap_link_cache("SDIS", &mut link.parsed);
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
                    a.set_limit(AlarmLimitField::Hihi, v);
                }
            }
            "HIGH" => {
                if let (Some(v), Some(a)) = (
                    AlarmLimit::from_stored(&value),
                    self.common.analog_alarm.as_mut(),
                ) {
                    a.set_limit(AlarmLimitField::High, v);
                }
            }
            "LOW" => {
                if let (Some(v), Some(a)) = (
                    AlarmLimit::from_stored(&value),
                    self.common.analog_alarm.as_mut(),
                ) {
                    a.set_limit(AlarmLimitField::Low, v);
                }
            }
            "LOLO" => {
                if let (Some(v), Some(a)) = (
                    AlarmLimit::from_stored(&value),
                    self.common.analog_alarm.as_mut(),
                ) {
                    a.set_limit(AlarmLimitField::Lolo, v);
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
            // Mirrors to common.text.out so the processing framework dispatches it.
            "OUTN" => {
                if self.record.record_type() != "swait" {
                    // No OUTN field on any other record type — the same
                    // `S_dbLib_fieldNotFound` the catch-all below reports.
                    return Err(self.unknown_field_error(name.into_owned()));
                }
                if let Some(link) = link {
                    std::mem::swap(&mut self.common.text.out, &mut link.text);
                    // Bare OUT link is NPP — see the "OUT" arm.
                    self.swap_link_cache("OUT", &mut link.parsed);
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
        let special = self.record.special(&name, true);
        match link {
            Some(link) if matches!(&*name, "INP" | "OUT") => {
                self.rebind_device_link(bound, link, special)?
            }
            _ => special?,
        }
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
    /// models no storage for — the WRITE owner of [`RecordCold::declared_overrides`]
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
                // A link the record leaves to the framework keeps its text
                // here and its parse in its slot, like any other.
                if let (Some(i), EpicsValue::String(text)) =
                    (self.link_slots.position(desc.name), &coerced)
                {
                    self.link_slots.install_text(i, desc, text.as_bytes());
                }
                self.cold
                    .declared_overrides
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
            "NAME" => Some(EpicsValue::String(self.name.as_bytes().into())),
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
        debug_assert!(
            self.udf_alarm.is_some() == self.record.raises_udf_alarm(),
            "a record that changes its UDF_ALARM guard after construction \
             cannot be served from ProcessPlan"
        );
        if let Some(udf) = self.udf_alarm {
            recgbl::rec_gbl_check_udf(&mut self.common, udf.exact_one, udf.severity, udf.message);
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
        if self.common.analog_alarm.is_some() {
            self.record.check_analog_alarms(&mut self.common);
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
    /// Arm the one-shot skip of this cycle's `do_sub`. Only a type that has
    /// one, so the flag is never left set on a record that never reads it.
    pub(crate) fn skip_subroutine_this_cycle(&mut self) {
        self.suppress_subroutine_run |= self.monitor_plan.subroutine_kind.is_some();
    }

    pub(crate) fn run_registered_subroutine(&mut self) -> CaResult<crate::error::DbResult> {
        let outcome = self.run_subroutine_body();
        // A subroutine that errored out has no C counterpart (a C subroutine
        // returns a `long`); it is a failed cycle, so it takes the non-zero
        // arm — no outputs.
        let status = *outcome.as_ref().unwrap_or(&SUBROUTINE_STATUS_ERROR);
        self.record.set_subroutine_status(status);
        outcome?;
        // `subRecord.c` returns this status, but 0 for the asynchronous 1;
        // `aSubRecord.c` returns 0.
        Ok(match self.monitor_plan.subroutine_kind {
            Some(SubroutineKind::Sub) if status != 1 => {
                crate::error::DbStatus::new(status as i32).map_or(Ok(()), Err)
            }
            _ => Ok(()),
        })
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

        // Only `sub` and `aSub` have a `do_sub` in their rset at all, and only
        // they are called (`ProcessPlan::runs_subroutine`); a type without one
        // must not take `do_sub`'s bad-sub exit.
        let Some(kind) = self.monitor_plan.subroutine_kind else {
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
        let status = self.call_subroutine(&sub_fn)?;

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
    /// ([`AuxPostMask`], [`MonitorValueGate`](crate::server::record::MonitorValueGate)) were already
    /// single-owned for the same reason — this is the loop around them.
    ///
    /// It also UPDATES `last_posted` for every post the cycle makes — including
    /// a post to a field nobody subscribes to, which it does not return — and
    /// it TAKES the
    /// record's per-cycle post mask ([`Record::take_cycle_posted_fields`]), so
    /// it must run exactly once per cycle.
    ///
    /// The rules, in order:
    ///
    /// * The deadband field (default VAL) is emitted by the caller with its own
    ///   C mask and is skipped here, as are every `dbCommon` field and the
    ///   [`MONITOR_TRACKER_FIELDS`] ([`walk_skip_term`]).
    /// * [`Record::event_posted_fields`] post from their own event path
    ///   (waveform HASH) — never from change detection.
    /// * [`Record::process_posted_fields`], when declared, is the closed set of
    ///   fields a process cycle may post at all.
    /// * A secondary value field ([`Record::fields_posted_with_value_mask`])
    ///   carries VAL's monitor mask, gated per its [`ValuePostGate`](super::ValuePostGate).
    /// * A CHANGED field carries [`AuxPostMask::mask_of_class`] of its class — unless it is a
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
        snapshot: &mut ProcessSnapshot,
        deadband_mask: EventMask,
        alarm_bits: EventMask,
        include_val: bool,
        marked: bool,
    ) {
        use crate::server::record::{CycleMarks, CyclePostMask, SecondaryTake, ValuePostGate};

        let hot = self.monitor_hot;
        let cycle_posted = hot.has(MonitorHot::CYCLE_POSTED);
        // C's secondary block, TAKEn once per cycle ahead of the mark TAKE. It
        // decides each field from the record's own old copy and advances it (C
        // aoRecord.c:535-549), whether or not anyone is subscribed, before the
        // walk reads the record; the posts land after the walk, in the order C
        // emits them. With VAL's mask shut only a forced block can open.
        let secondary = if hot.has(MonitorHot::FORCED_SECONDARY)
            || !deadband_mask.is_empty() && hot.has(MonitorHot::VALUE_MASKED)
        {
            self.take_secondary(deadband_mask)
        } else {
            SecondaryTake::NONE
        };
        // One mask for the cycle nobody watches: no block fired, no mark to
        // TAKE, no field for the walk. An own-posts type falls through to the
        // exact test below.
        if secondary.is_none() && !marked && !hot.has(MonitorHot::WALK_OWED) {
            return;
        }
        // TAKE — this also clears the state it answers from (C's
        // `pcalc->newm = 0`), which is why it may run only once per cycle.
        let cycle_marks = if marked || cycle_posted {
            let marks = self.record.take_cycle_posted_fields();
            debug_assert!(
                !self.record.cycle_posts_remain(),
                "the monitor walk takes once"
            );
            marks
        } else {
            CycleMarks::NONE
        };
        let secondary_fired = secondary.fired();
        let any_marks = !cycle_marks.is_empty();
        let value_masked = self.monitor_plan.value_masked;
        // The walk has no field to reach — nothing subscribed and none it can
        // act on ever published, or a type answering its own posts with no mark
        // — and no forced post fired, so there is nothing to advance. The published values of the fields
        // the walk always skips live in `skipped_posted`, so this is exact:
        // while they shared `last_posted` an alarm transition left it
        // permanently non-empty and the walk ran forever for nothing. The record's declared post sets are the walk's
        // inputs alone, so asking for them, six more trips through the
        // vtable, is asked only here. Everything a cycle owes whether or not
        // anyone is watching — the TAKEs above and the record's own old-copy
        // advance — has already run.
        if secondary_fired == 0
            && self.walk_unreached()
            && !(hot.has(MonitorHot::ANSWERS_OWN) && any_marks)
        {
            return;
        }
        #[cfg(test)]
        WALKS.with(|n| n.set(n.get() + 1));
        // Every post this walk adds sits at or past this index, so the
        // published-state advance at the tail covers exactly those.
        let first_post = snapshot.len();

        // C's default for a change-detected auxiliary post:
        // `monitor_mask | DBE_VALUE | DBE_LOG` (calcRecord.c:420, subRecord.c:400;
        // motor `DBE_VAL_LOG` for marked fields, motorRecord.cc:3522-3645).
        let aux_mask = alarm_bits | EventMask::VALUE | EventMask::LOG;
        // Each field's answers to the lists below are settled bits
        // ([`Roles`]); a list is asked here only so a turned-over one
        // resettles them. The alarm fan-out list is read only on an alarm
        // cycle, and the mark table only on a marked one.
        let settled = self.posted.lists();
        let fanout_open = !alarm_bits.is_empty();
        let alarm_fanout = if fanout_open {
            self.record.alarm_cycle_monitored_fields()
        } else {
            settled.fanout
        };
        let alarm_fanout_mask =
            if fanout_open && !alarm_fanout.is_empty() && self.record.alarm_cycle_posts_value_log()
            {
                aux_mask
            } else {
                alarm_bits
            };
        let sets = self.has_hook(crate::server::record::CycleHooks::MONITOR_SETS);
        self.settle_roles(Lists {
            forced: if sets {
                self.record.force_posted_fields()
            } else {
                &[]
            },
            fanout: alarm_fanout,
            swept: if sets {
                self.record.log_swept_fields()
            } else {
                &[]
            },
            marks: if any_marks {
                cycle_marks.table()
            } else {
                settled.marks
            },
        });
        let process_posted = self.settle_posted();
        let prev_changed = if sets {
            self.record.prev_compared_inputs().1
        } else {
            0
        };

        // The walk covers every field whose published value is tracked — each
        // subscribed field, and each field a post has already published — not
        // only the fields someone watches now. C `monitor()` decides from the
        // record's own state and `db_post_events` with no subscriber leaves
        // that state advanced, so what the record has published must not
        // depend on who is watching: a move finished with no `.DMOV` monitor
        // left `last_posted` at the move-start 0 while the field read 1, and
        // the next subscriber's move-start 0 compared equal and was dropped.
        // A field with no entry has never been published; `add_subscriber`
        // seeds it from the value the subscriber is handed.
        // Published fields first, read from the entry the iterator hands over;
        // only a subscribed field never published costs a probe.
        // A field two monitors watch is walked once, at its first monitor.
        let tracked = self
            .last_posted
            .entries()
            .map(|(field, key, declared, published)| {
                let posted = published.posted();
                (
                    field,
                    key,
                    Some(published),
                    declared,
                    false,
                    posted,
                    published.roles(),
                )
            })
            .chain(
                self.subscribers
                    .iter()
                    .filter(|sub| sub.walk != SubWalk::Skip)
                    .map(|sub| {
                        let declared = match sub.name {
                            SubName::Declared(desc) => Some(desc),
                            SubName::Named(_) => None,
                        };
                        let claimed = sub.walk == SubWalk::Claimed;
                        (
                            sub.field(),
                            sub.key(),
                            None,
                            declared,
                            claimed,
                            sub.posted,
                            &sub.roles,
                        )
                    }),
            );
        for (field, key, published, declared, claimed, posted, roles) in tracked {
            // `walk_skip_term` can only fire on a field never published, and
            // `settle_sub_walk` leaves such a monitor out of the walk.
            if process_posted.is_some() && !posted {
                continue;
            }
            // A claimed field is never published, so nothing compares it: only
            // a post the type or a declared set owes it reads it.
            let fanout = fanout_open && roles.fanout();
            if claimed
                && !matches!(roles.gate, Some(ValuePostGate::WithValue))
                && !roles.forced()
                && !cycle_marks.any_among(roles.marks)
                && !fanout
                && !roles.swept()
            {
                continue;
            }
            // A reference-log field is never read for its post: `val` is
            // `None` for it, and the post is made from `posted` alone. `prev`
            // is the published value a read one compares against.
            let (val, prev) = match published {
                Some(p) => match &p.value {
                    PostedValue::Ref => (None, None),
                    PostedValue::Val(prev) => {
                        let val = if p.upper {
                            self.resolve_field_upper_at(field, p.desc)
                        } else {
                            self.resolve_field(field)
                        };
                        if val.is_none() {
                            continue;
                        }
                        (val, Some(prev))
                    }
                },
                None => match self.first_post_payload(key, field) {
                    Some(PostedValue::Val(val)) => (Some(val), None),
                    Some(PostedValue::Ref) => (None, None),
                    None => continue,
                },
            };
            // Only a field never published, or one keyed off its declared
            // spelling, copies its name into a post.
            let name = || match declared {
                Some(desc) => PostName::Declared(desc),
                None => PostName::Named(Cow::Owned(field.to_owned())),
            };
            // C keeps no published copy of a reference-log field to compare
            // against; only its record's own old copy (`prev_bit`) or a mark
            // posts it.
            let changed = !claimed && {
                // A subscribed field is published when it is added, so the name
                // search runs only for one never posted since.
                let prev_bit = match published {
                    Some(p) => p.prev_bit,
                    None => self.monitor_plan.fixed_roles(key).prev_bit,
                };
                match (prev_bit, &val) {
                    (Some(bit), _) => prev_changed & (1 << bit) != 0,
                    (None, None) => false,
                    (None, Some(val)) => match prev {
                        Some(prev) => prev != val,
                        None => self
                            .posted_value_at(key)
                            .is_none_or(|prev| prev.as_value() != Some(val)),
                    },
                }
            };
            if let Some(gate) = roles.gate {
                // C posts this secondary value field with VAL's own monitor_mask,
                // from inside the guard that decides whether VAL posts at all —
                // never a forced DBE_VALUE|DBE_LOG. `ValuePostGate` says whether C
                // also re-tests the field's own value inside that guard (ai RVAL,
                // aiRecord.c:462) or posts it whenever the guard fires (timestamp
                // RVAL, timestampRecord.c:160).
                let post = match gate {
                    ValuePostGate::WithValue => include_val,
                    // Decided once per cycle in `secondary_fired` above,
                    // against the record's own old copy — never here, where
                    // the answer would depend on this loop's `last_posted`
                    // cache and on the field having a subscriber.
                    ValuePostGate::OnChange
                    | ValuePostGate::OnChangeForced
                    | ValuePostGate::Guarded => false,
                };
                if post {
                    snapshot.push((name(), PostedValue::of_read(&val), deadband_mask));
                }
            } else if changed {
                snapshot.push((
                    name(),
                    PostedValue::of_read(&val),
                    roles.aux_mask(alarm_bits, deadband_mask),
                ));
            } else if roles.forced() {
                // C `monitor()` posts a statically re-marked field with
                // `monitor_mask | DBE_VAL_LOG` even when unchanged.
                snapshot.push((name(), PostedValue::of_read(&val), aux_mask));
            } else if cycle_marks.any_among(roles.marks) {
                // One event per MARK, each with the mask of the C call site that
                // made it (`CyclePostMask`) — a field marked twice (aCalcout's
                // AMASK `afterCalc` post AND its NEWM `monitor()` post) is posted
                // twice, exactly as C posts it from both loops.
                for cycle_mask in cycle_marks.masks_among(roles.marks) {
                    let mask = match cycle_mask {
                        CyclePostMask::Value => EventMask::VALUE,
                        CyclePostMask::ValueLog => EventMask::VALUE | EventMask::LOG,
                        CyclePostMask::MonitorValueLog
                        | CyclePostMask::ResetValueLog
                        | CyclePostMask::ResetValueLogOnAlarm => aux_mask,
                        CyclePostMask::ResetAlarm if alarm_bits.is_empty() => continue,
                        CyclePostMask::ResetAlarm => alarm_bits,
                        CyclePostMask::MonitorValue => deadband_mask | EventMask::VALUE,
                        CyclePostMask::MonitorLog => deadband_mask | EventMask::LOG,
                    };
                    snapshot.push((name(), PostedValue::of_read(&val), mask));
                }
            } else if fanout {
                // C motor `monitor()` (motorRecord.cc:3456-3646) posts every listed
                // field once `monitor_mask != 0`, so a DBE_ALARM-only subscriber
                // observes the alarm moment on any of them.
                snapshot.push((name(), PostedValue::of_read(&val), alarm_fanout_mask));
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
            if roles.swept() {
                snapshot.push((
                    name(),
                    PostedValue::of_read(&val),
                    EventMask::LOG | alarm_bits,
                ));
            }
        }
        if secondary_fired != 0 {
            self.post_forced_secondaries(
                snapshot,
                value_masked,
                secondary_fired,
                deadband_mask | secondary.widen(),
                deadband_mask,
            );
        }
        // Every post the cycle made is published whether or not anyone
        // watches the field: advance the published values first, then hand on
        // only the posts a subscriber receives. C's `db_post_events` with no
        // subscriber delivers nothing, and still leaves the record's state
        // advanced. `snapshot` is the caller's, not a field of `self`, so the
        // walk's pushes and this advance do not contend for the record.
        for (field, val, _) in snapshot.iter().skip(first_post) {
            let key = match field {
                PostName::Declared(desc) => self.monitor_plan.key_of(desc),
                PostName::Named(name) => self.post_key(name),
            };
            self.record_value_post_as(key, field, val.clone());
        }
        snapshot.retain_from(first_post, |(field, _, _)| {
            self.subscribers.iter().any(|sub| sub.watches(field))
        });
    }

    /// [`Record::take_secondary_value_changes`], once per cycle.
    #[inline(always)]
    fn take_secondary(&mut self, deadband_mask: EventMask) -> crate::server::record::SecondaryTake {
        #[cfg(test)]
        SECONDARY_TAKES.with(|n| n.set(n.get() + 1));
        let secondary = self.record.take_secondary_value_changes(deadband_mask);
        debug_assert!(
            self.monitor_plan.secondary_fits(secondary, deadband_mask),
            "{} fired a secondary its gate keeps shut",
            self.record.record_type()
        );
        secondary
    }

    /// The secondary posts `collect_subscriber_posts` decided fired. A guarded
    /// secondary post lands whether or not the field has a subscriber, as C
    /// calls `db_post_events` (aoRecord.c:536-549); one nobody watches only
    /// advances its published value, since pushed it would spill the snapshot
    /// to the heap for a post `retain` drops. Out of line: inlined, it cost
    /// every unmonitored ai/calc cycle 6-9 instructions it never runs.
    #[inline(never)]
    fn post_forced_secondaries(
        &mut self,
        snapshot: &mut ProcessSnapshot,
        value_masked: &'static [(&'static str, crate::server::record::ValuePostGate)],
        fired: u64,
        guard: EventMask,
        deadband_mask: EventMask,
    ) {
        use crate::server::record::ValuePostGate;
        let silent = self.monitor_plan.value_masked_silent;
        let fired = if self.unwatched() {
            fired & !silent
        } else {
            fired
        };
        for (index, (name, gate)) in value_masked.iter().enumerate() {
            if fired & (1 << index) == 0 {
                continue;
            }
            #[cfg(test)]
            FORCED_SECONDARY_VISITS.with(|n| n.set(n.get() + 1));
            let mask = match gate {
                ValuePostGate::OnChangeForced => guard | EventMask::VALUE | EventMask::LOG,
                ValuePostGate::Guarded => guard,
                _ => deadband_mask,
            };
            let declared = self
                .monitor_plan
                .value_masked_declared
                .get(index)
                .copied()
                .flatten();
            let posted = post_name(declared, name);
            let subscribed = self.subscribers.iter().any(|sub| sub.watches(&posted));
            // A claimed field keeps no published copy, so an unwatched post
            // of one is nothing at all.
            if !subscribed && silent & (1 << index) != 0 {
                continue;
            }
            let val = if self.monitor_plan.value_masked_ref & (1 << index) != 0 {
                PostedValue::Ref
            } else {
                let val = match declared {
                    Some(desc) => self.read_declared(desc),
                    None => self.resolve_field(name),
                };
                let Some(val) = val else {
                    continue;
                };
                PostedValue::Val(val)
            };
            if subscribed {
                snapshot.push((posted, val, mask));
            } else {
                let key = match declared {
                    Some(desc) => self.monitor_plan.key_of(desc),
                    None => self.post_key(name),
                };
                self.record_value_post_as(key, name, val);
            }
        }
    }

    /// The posts an `AsyncPendingNotify` pass publishes — the single owner both
    /// dispatch paths call (`processing.rs`'s engine and [`Self::process_local`]),
    /// so a mid-async post cannot obey one rule on one path and another rule on
    /// the other.
    ///
    /// Each post carries `DBE_VALUE|DBE_LOG`: C motor's mid-move
    /// `db_post_events` calls use `DBE_VAL_LOG` (motorRecord.cc:2606 DMOV, and
    /// every other `do_work` post), and no alarm transition ran on this pending
    /// pass.
    ///
    /// The deadband field is NOT change-detected against `last_posted` here.
    /// Whether it posts, with which mask, and where MLST/ALST land belong to
    /// [`Self::value_include_classes`] and [`Self::deadband_post`] on this pass
    /// as on every other — C motor `monitor()` runs on the move-start pass too
    /// (motorRecord.cc:1507) and posts RBV only on an MDEL/ADEL crossing,
    /// moving `mlst` to RBV (motorRecord.cc:3468-3507). `deadband_post`
    /// deliberately does not advance `last_posted`: MLST/ALST are where that
    /// field's published value lives, so change-detecting it here compared
    /// against a cache nothing maintains and re-posted the PREVIOUS readback at
    /// every move start.
    pub(crate) fn collect_notify_posts(&mut self, fields: Vec<FieldIx>) -> ProcessSnapshot {
        let deadband = self.monitor_plan.deadband_member();
        let mut posts = ProcessSnapshot::new();
        for ix in fields {
            let Some(desc) = self.monitor_plan.desc_at(ix) else {
                continue;
            };
            if Some(ix) == deadband {
                // The record's own value, not the notify's copy of it: C posts
                // the field itself (`db_post_events(pmr, &pmr->rbv, ...)`).
                let (include_val, include_archive, _) = self.value_include_classes();
                // No alarm bits: `recGblResetAlarms` has not run on this pending
                // pass, so the post carries only the classes MDEL/ADEL fired.
                self.deadband_post(&mut posts, EventMask::NONE, include_val, include_archive);
                continue;
            }
            // A reference-log field has no published copy to compare the
            // record's value against, so the record's notify posts it.
            let key = PostKey::Ix(ix);
            let Some(posted) =
                self.post_payload_at(key, |_| Some(desc), |i| i.record.get_field(desc.name))
            else {
                continue;
            };
            if posted == PostedValue::Ref || self.posted_value_at(key) != Some(&posted) {
                self.record_value_post_as(key, desc.name, posted.clone());
                posts.push((
                    PostName::Declared(desc),
                    posted,
                    EventMask::VALUE | EventMask::LOG,
                ));
            }
        }
        posts
    }

    /// C's soft input dset returns 2 ("don't convert"): `devAiSoft` and its
    /// kin for "Soft Channel" (and async soft), never "Raw Soft Channel",
    /// whose read lands in RVAL and must convert.
    pub(crate) fn soft_input_skips_convert(&self) -> bool {
        input_dset_skips_convert!(
            self.common.dtyp.skips_input_convert(),
            self.record.can_device_write(),
            self.record.soft_channel_skips_convert()
        )
    }

    /// **The single owner of a refused re-entry** — C `dbProcess`'s
    /// `MAX_LOCK` branch (`dbAccess.c:544-555`), whole: the `lcnt` count,
    /// the once-only SCAN_ALARM/INVALID "Async in progress" raise and its
    /// posts, which `recGblResetAlarms` makes for SEVR, STAT, AMSG and ACKS
    /// before the VAL post it returns `DBE_ALARM` for.
    ///
    /// Returns the posts for the caller to deliver after its write guard is
    /// gone, or `None` where C goes to `all_done` without a post.
    pub(crate) fn max_lock_refusal(&mut self) -> Option<ProcessSnapshot> {
        use crate::server::recgbl::{self, EventMask};
        const MAX_LOCK: i16 = 10;

        // The increment is in C's test, so every refused entry counts.
        let lcnt = self.common.lcnt;
        self.common.lcnt = lcnt.saturating_add(1);
        if self.common.stat == recgbl::alarm_status::SCAN_ALARM
            || lcnt < MAX_LOCK
            || self.common.sevr >= AlarmSeverity::Invalid
        {
            return None;
        }
        recgbl::rec_gbl_set_sevr_msg(
            &mut self.common,
            recgbl::alarm_status::SCAN_ALARM,
            AlarmSeverity::Invalid,
            "Async in progress",
        );
        let alarm_result = recgbl::rec_gbl_reset_alarms(&mut self.common);
        let mut posts = ProcessSnapshot::new();
        crate::server::database::alarm_field_posts(&self.common, &alarm_result).for_each(
            |desc, mask| {
                if let Some(value) = self.post_payload_at(
                    self.monitor_plan.key_of(desc),
                    |_| Some(desc),
                    |i| i.resolve_field_upper_at(desc.name, Some(desc)),
                ) {
                    posts.push((PostName::Declared(desc), value, mask));
                }
            },
        );
        let declared = self.monitor_plan.val_desc;
        let key = match declared {
            Some(desc) => self.monitor_plan.key_of(desc),
            None => self.post_key("VAL"),
        };
        if let Some(val) = self.post_payload_at(
            key,
            |i| declared.or_else(|| i.field_desc("VAL")),
            |i| i.record.val(),
        ) {
            posts.push((
                post_name(declared, "VAL"),
                val,
                EventMask::VALUE | EventMask::LOG | EventMask::ALARM,
            ));
        }
        Some(posts)
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
        if self.pact.swap(true, std::sync::atomic::Ordering::AcqRel) {
            let posts = self.max_lock_refusal().unwrap_or_default();
            return Ok((posts, Vec::new()));
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
        // `process_local`'s callers never read a status.
        let _ = self.run_registered_subroutine()?;
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
        if self.soft_input_skips_convert() {
            self.record.set_device_did_compute(true);
        }
        // `process_and_drain` pushes framework-owned common state (UDF/PHAS/TSE/TSEL) so the
        // record's process() can see it — same as the processing.rs link
        // path. `process_local` is the foreign-call path
        // (`db.process_record`); without this a record driven through it
        // (e.g. QSRV group-process members) would not see UDF/TSE.
        let outcome = self.process_and_drain()?;
        let process_result = outcome.result;
        // Note: process_local() does not execute ProcessActions — those are
        // handled by the full process_record_with_links() path in processing.rs
        // — save the `PutAfterEpilogue`s, applied after `monitor()` below.
        //
        // It must still apply the `PutInLinkStage` stores and `Trace`s. There
        // are no link writes here for them to be ordered against, so they
        // apply now; what
        // is NOT optional is applying them at all — a record that hands its
        // completion-flag clear to the framework (sseq's `busy`) would
        // otherwise stay busy forever on this path.
        let after_epilogue = outcome.actions;
        for action in &after_epilogue {
            match *action {
                crate::server::record::ProcessAction::PutInLinkStage { field, value, post } => {
                    self.put_in_link_stage(field, value, post);
                }
                crate::server::record::ProcessAction::Trace { prefix, suffix } => {
                    let name = String::from_utf8_lossy(self.name.as_bytes());
                    println!("{prefix}{name}{suffix}");
                }
                _ => {}
            }
        }

        // If the record reports it modified a metadata-class field during
        // process(), rebuild the metadata cache from the new values.
        // Default impl returns false, so
        // most records pay zero cost here.
        if self.record.took_metadata_change() {
            self.refresh_metadata_cache();
            // mirror C db_post_events(precord, NULL, DBE_PROPERTY) after record processing.
            self.post_record_wide(
                &mut PostBurst::new(),
                crate::server::recgbl::EventMask::PROPERTY,
            );
        }

        if process_result == RecordProcessResult::AsyncPending {
            // Async: PACT stays set, no further processing this cycle
            // Don't clear processing flag (guard won't run — we leak it intentionally)
            std::mem::forget(_guard);
            return Ok((ProcessSnapshot::new(), Vec::new()));
        }
        if let RecordProcessResult::AsyncPendingNotify(fields) = process_result {
            // Intermediate notification (e.g. DMOV=0 at move start).
            // Unlike AsyncPending, we DO release the processing flag so
            // subsequent I/O Intr cycles can continue processing normally.
            self.common.time = crate::runtime::general_time::get_current();
            // The pass's posts, through the owner this path shares with the
            // engine (`Self::collect_notify_posts`).
            let changed_fields = self.collect_notify_posts(fields);
            // _guard drops here, clearing the processing flag
            return Ok((changed_fields, Vec::new()));
        }
        if let RecordProcessResult::CompleteNoEmit { raise } = process_result {
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
            if let Some((stat, sevr)) = raise {
                crate::server::recgbl::rec_gbl_set_sevr(&mut self.common, stat, sevr);
            }
            return Ok((ProcessSnapshot::new(), Vec::new()));
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
        // Stamped before the alarm hooks, as `process_cycle` does (C
        // aiRecord.c:156/164).
        let _begun = self.common.begin_stamp();
        self.common.time = crate::runtime::general_time::get_current();

        // Per-record alarm hook (C `checkAlarms()`).
        self.record.check_alarms(&mut self.common);

        // Evaluate alarms (accumulates into nsta/nsev)
        self.evaluate_alarms();
        // UDF already updated above — do not clear unconditionally.

        let mut snapshot = ProcessSnapshot::new();
        let alarm_posts = self.monitor_cycle(&mut snapshot);
        // The engine's tail runs these after `recGblFwdLink`; this path has no
        // forward link, so `monitor()` is the last site they follow.
        for action in after_epilogue {
            if let crate::server::record::ProcessAction::PutAfterEpilogue { field, value } = action
            {
                self.put_after_epilogue(field, value, alarm_posts.reset_mask());
            }
        }

        Ok((snapshot, alarm_posts.to_vec()))
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
    /// C `monitor()`'s VALUE / LOG gate for the primary-value post —
    /// `(include_val, include_archive)`, the single owner every processing path
    /// feeds into [`Self::deadband_post`] and [`Self::collect_subscriber_posts`].
    /// Keeping it in one place is what stops the rule from holding on the
    /// synchronous path but not the async-continuation / put-notify paths.
    ///
    /// The third answer is [`MonitorValueClasses::marked`](crate::server::record::MonitorValueClasses::marked).
    #[inline(always)]
    pub(crate) fn value_include_classes(&mut self) -> (bool, bool, bool) {
        // fanout/seq "trigger" records post VAL only with the alarm events
        // `recGblResetAlarms` returns, never DBE_VALUE/DBE_LOG — see
        // `Record::process_posts_value_monitor`. The alarm bits still reach VAL
        // via `deadband_post`'s `alarm_bits`, so an alarm transition still posts
        // it; only the value/archive classes are suppressed.
        if !self.monitor_hot.has(MonitorHot::POSTS_VALUE) {
            return (false, false, false);
        }
        // lsi/lso and the binary records gate on their own change test (C
        // `lsiRecord.c`/`lsoRecord.c` monitor: `len != olen || memcmp(oval,
        // val, len)`), OR-ing in the MPST/APST `menuPost` "Always" override;
        // a type with neither a change test nor a deadband posts always.
        let mut last_posted = [self.common.mlst, self.common.alst];
        let classes = self
            .record
            .monitor_value_classes(self.monitor_hot.value_gate(), &mut last_posted);
        [self.common.mlst, self.common.alst] = last_posted;
        self.array_hash_changed = classes.hash_changed;
        (classes.value, classes.archive, classes.marked)
    }

    /// C `monitor()`'s post of the deadband field, into `snapshot`. Returns
    /// C's `monitor_mask`, which the [`Record::fields_posted_with_value_mask`]
    /// secondaries ride: C posts them inside the same `if (monitor_mask)`.
    #[inline(always)]
    pub(crate) fn deadband_post(
        &mut self,
        snapshot: &mut ProcessSnapshot,
        alarm_bits: EventMask,
        include_val: bool,
        include_archive: bool,
    ) -> EventMask {
        let hot = self.monitor_hot;
        let mut mask = alarm_bits;
        if include_val {
            mask |= EventMask::VALUE;
        }
        if include_archive && !hot.has(MonitorHot::LOG_SUPPRESSED) {
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
        if mask.is_empty()
            || !(hot.has(MonitorHot::ALARM_ALONE) || include_val || include_archive)
            || !self.deadband_posted()
        {
            return mask;
        }
        if self.unwatched() {
            // `notify_from_snapshot`'s gate, taken before the read: the post
            // has no one to deliver to, and the value is a copy (an
            // allocation for compress, waveform and the string records).
            snapshot.push_unwatched(mask);
            return mask;
        }
        let MonitorPlan {
            deadband_field: field,
            deadband_is_val,
            ..
        } = self.monitor_plan;
        // A reference-log field's post carries no value: it is read at
        // delivery, as C's `dbfl_type_ref` log is.
        let value = if self.monitor_plan.deadband_ref {
            Some(PostedValue::Ref)
        } else if deadband_is_val {
            self.record.val().map(PostedValue::Val)
        } else {
            self.resolve_field_upper_at(field, self.monitor_plan.deadband_desc)
                .map(PostedValue::Val)
        };
        if let Some(value) = value {
            snapshot.push((
                post_name(self.monitor_plan.deadband_declared, field),
                value,
                mask,
            ));
        }
        mask
    }

    /// Whether the deadband field is in the record's
    /// [`Record::process_posted_fields`] now, asked of the record's own list
    /// and the plan's index in [`MonitorHot`].
    #[inline(always)]
    fn deadband_posted(&self) -> bool {
        let member = self.monitor_hot.deadband_member();
        self.record
            .process_posted_fields()
            .is_none_or(|list| holds(list, member))
    }

    #[inline(always)]
    pub fn check_deadband_ext(&mut self) -> (bool, bool) {
        // The deadband is evaluated against `monitor_deadband_value()`, not
        // `val()` directly: the motor record deadbands RBV, its readback.
        let mut last_posted = [self.common.mlst, self.common.alst];
        let classes = crate::server::record::monitor_deadband_classes(
            &mut *self.record,
            self.monitor_plan.value_gate.array_monitor_post,
            &mut last_posted,
        );
        [self.common.mlst, self.common.alst] = last_posted;
        self.array_hash_changed = classes.hash_changed;
        (classes.value, classes.archive)
    }

    /// The record's half of every field log a post makes now
    /// (`db_create_field_log`, `dbEvent.c:704-713`). `always`: every
    /// monitored post runs it, and out of line it cost +32 instr/cycle.
    #[inline(always)]
    fn log_head(&self) -> crate::server::pv::LogHead {
        #[cfg(test)]
        HEADS.with(|n| n.set(n.get() + 1));
        crate::server::pv::LogHead {
            stat: self.common.stat,
            sevr: self.common.sevr as u16,
            amsg: crate::server::pv::log_record_amsg(&self.common.amsg),
            time: self.common.time,
            utag: self.common.utag as i32,
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
        let limit = |name: &str| self.metadata_limit(name);
        // The raw stored ordinal, NOT clamped to 0..=3: C tests `prec->hhsv`
        // for NONZERO, so an out-of-range severity still enables its limit.
        let severity = |name: &str| {
            self.metadata_field(name)
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
        let f: &str = &upper_field(field);
        let bands: &[&str] = match rtype {
            "dfanout" => &["LALM", "ALST", "MLST"],
            "acalcout" | "scalcout" | "epid" => &["HIHI", "HIGH", "LOW", "LOLO"],
            "waveform" | "aai" | "aao" | "compress" | "histogram" | "subArray" | "motor" => &[],
            _ => &["HIHI", "HIGH", "LOW", "LOLO", "LALM", "ALST", "MLST"],
        };
        if bands.contains(&f) {
            return true;
        }
        match rtype {
            // sel's args are 12 (`SEL_MAX`), not the calc family's 21.
            "sel" => Self::calc_arg_field(f, 12),
            "acalcout" | "scalcout" => {
                Self::calc_arg_field(f, 12)
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
        let f: &str = &upper_field(field);
        let bands: &[&str] = match rtype {
            "acalcout" | "scalcout" | "epid" => &["HIHI", "HIGH", "LOW", "LOLO"],
            // `swaitRecord.c:597-606` is a bare `pfield == &pwait->val` test,
            // so its `ALST`/`MLST` take `recGblGetGraphicDouble` — and swait
            // has no HIHI/HIGH/LOW/LOLO to ask about.
            "swait" => &[],
            _ => &["HIHI", "HIGH", "LOW", "LOLO", "LALM", "ALST", "MLST"],
        };
        if bands.contains(&f) {
            return true;
        }
        match rtype {
            "ai" | "longin" | "int64in" => f == "SVAL",
            "ao" => matches!(f, "OVAL" | "PVAL" | "IVOV"),
            "compress" => matches!(f, "IHIL" | "ILIL"),
            // sel's args are 12 (`SEL_MAX`), not the calc family's 21.
            "sel" => Self::calc_arg_field(f, 12),
            // A..L and PA..PL, both to HOPR/LOPR.
            "acalcout" | "scalcout" => {
                Self::calc_arg_field(f, 12)
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
        let f: &str = &upper_field(field);
        match rtype {
            // `case VAL: case OVAL: case PVAL: break;` (`aoRecord.c:305-312`).
            "ao" => matches!(f, "OVAL" | "PVAL"),
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
            "histogram" => matches!(f, "ULIM" | "LLIM" | "SGNL" | "SVAL" | "WDTH"),
            // `BPTR` returns, and the four calibration fields answer a literal
            // 6 (`mcaRecord.c:898-905`).
            "mca" => matches!(f, "BPTR" | "CALO" | "CALS" | "CALQ" | "TTH"),
            // `case RRBV: case RMP: case REP: *precision = 0; break;` and
            // `case VERS: *precision = 2; break;` — both `break` past the
            // switch to the bare `return`, never to `recGblGetPrec`
            // (`motorRecord.cc:3322-3330`).
            "motor" => matches!(f, "RRBV" | "RMP" | "REP" | "VERS"),
            // `sel` is deliberately absent: its `A`..`L` / `LA`..`LL` loop
            // compares `paddr->pfield` against `&pvalue` and `&plvalue` — the
            // addresses of the two LOCAL pointers, not the fields they walk
            // (`selRecord.c:159-160`) — so the test never matches and every
            // `sel` argument reaches `recGblGetPrec`. Transcribed as C
            // behaves, not as it reads.
            _ => false,
        }
    }

    /// Notify subscribers from a snapshot (call outside lock).
    /// Each entry carries its own posting mask: only subscribers whose
    /// mask intersects that field's mask are notified, and the delivered
    /// [`MonitorEvent`](crate::server::pv::MonitorEvent) reports that intersection — C
    /// `db_post_events(prec, &field, mask)` per-field granularity, then
    /// `pLog->mask = caEventMask & pevent->select` per subscriber.
    #[inline(always)]
    pub fn notify_from_snapshot(&self, snapshot: &ProcessSnapshot) {
        // Nothing is subscribed to any field, so no post below can land; the
        // per-field lookup is a hash of the field name per post per cycle to
        // find that out. Same gate as `collect_subscriber_posts`, kept in the
        // inlined half so the unmonitored cycle pays the test and not the
        // delivery loop's frame. A cycle that posts nothing likewise skips the
        // frame, whose log head and burst it would set up for no entry.
        if snapshot.is_empty() || self.unwatched() {
            return;
        }
        self.deliver_snapshot(snapshot);
    }

    /// Drops `field`'s monitors whose consumer is gone; how many remain.
    /// Out of line, so a put's unwatched test keeps its frame.
    #[inline(never)]
    fn reap_closed(&mut self, key: PostKey<'_>) -> usize {
        let mut live = 0;
        self.retain_subscribers(|sub| {
            if !sub.watches_key(key) {
                return true;
            }
            let open = !sub.sub.is_closed();
            live += usize::from(open);
            open
        });
        live
    }

    /// The one way a monitor leaves the list: a removal re-settles the walk,
    /// since the field's next monitor may now be its first.
    fn retain_subscribers(&mut self, mut keep: impl FnMut(&FieldSubscriber) -> bool) {
        let before = self.subscribers.len();
        self.subscribers.retain(|sub| keep(sub));
        if self.subscribers.len() != before {
            self.set_subscribers_watched();
            self.settle_sub_walk();
        }
    }

    /// Settle every monitor's [`SubWalk`] and `posted`. Run when the list
    /// changes, when a field is first published and when the
    /// [`Record::process_posted_fields`] answer turns over — never per cycle.
    #[cold]
    #[inline(never)]
    fn settle_sub_walk(&mut self) {
        let plan = &self.monitor_plan;
        for at in 0..self.subscribers.len() {
            let sub = &self.subscribers[at];
            let key = sub.key();
            let (walk_skip, claimed) = match key {
                PostKey::Ix(ix) => (plan.walk_skip_bits.has(ix), plan.claimed_bits.has(ix)),
                PostKey::Named(name) => (
                    walk_skip_term(name, plan.deadband_field, self.record.event_posted_fields()),
                    names_contain(plan.claimed, name),
                ),
            };
            let walk = if self.subscribers[..at]
                .iter()
                .any(|first| first.watches_as(sub))
                || walk_skip
            {
                SubWalk::Skip
            } else if claimed {
                SubWalk::Claimed
            } else if self.last_posted.contains(key) {
                SubWalk::Skip
            } else {
                SubWalk::Unpublished
            };
            let posted = self.posted.contains(sub.member());
            let sub = &mut self.subscribers[at];
            sub.walk = walk;
            sub.posted = posted;
        }
    }

    /// Whether the cycle walk has no field to reach unless a mark or a fired
    /// secondary gives it one. A type that answers its own posts never has,
    /// so who is subscribed is not asked for it; any other reaches its
    /// subscribed and published fields.
    #[inline(always)]
    fn walk_unreached(&self) -> bool {
        let hot = self.monitor_hot;
        debug_assert_eq!(hot.has(MonitorHot::PUBLISHED), !self.last_posted.is_empty());
        hot.has(MonitorHot::ANSWERS_OWN) || self.unwatched() && !hot.has(MonitorHot::PUBLISHED)
    }

    /// No monitor is attached, read from [`MonitorHot`] rather than the list.
    #[inline(always)]
    fn unwatched(&self) -> bool {
        #[cfg(test)]
        SUBSCRIBER_TESTS.with(|n| n.set(n.get() + 1));
        debug_assert_eq!(
            self.monitor_hot.has(MonitorHot::WATCHED),
            !self.subscribers.is_empty()
        );
        !self.monitor_hot.has(MonitorHot::WATCHED)
    }

    /// The one writer of [`MonitorHot::WATCHED`], run after every change to
    /// the list.
    fn set_subscribers_watched(&mut self) {
        self.monitor_hot
            .set(MonitorHot::WATCHED, !self.subscribers.is_empty());
    }

    #[inline(never)]
    fn deliver_snapshot(&self, snapshot: &ProcessSnapshot) {
        #[cfg(test)]
        DELIVERIES.with(|n| n.set(n.get() + 1));
        // Same ambient-origin inheritance as `notify_field_with_origin`:
        // a process cycle driven by an in-process writer's put tags its
        // posts with the writer's origin, so the writer's own filtered
        // subscriptions do not hear its cascade. 0 outside any scope.
        let origin = ambient_write_origin();
        // The cycle's posts are one burst: no reader wakes before the last
        // is queued, as none could take C's `dbScanLock` until it was.
        let mut burst = crate::server::event_queue::PostBurst::new();
        // Built for the first post that passes a mask, as C builds a log only
        // past `select` (`dbEvent.c:896-900`): an attached cycle that posts
        // nothing cuts no amsg.
        let mut head = None;

        for (field, value, posting_mask) in snapshot.iter() {
            let posting_mask = *posting_mask;
            // C `db_post_events` walks `mlis` matching the field
            // (`dbEvent.c:888-890`). Each subscriber's log copies the record's
            // alarm, time and utag and a scalar value; a wider one is read at
            // delivery (`dbEvent.c:704-730`). No metadata rides on a post: the
            // consumer reads it when it serves a class that carries it.
            let mut logged = None;
            for entry in self.subscribers.iter().filter(|sub| sub.watches(field)) {
                let sub = &entry.sub;
                // Paused subscriber (`db_event_disable`): suppress at
                // the source — no delivery, no coalesce.
                if !sub.is_active() {
                    continue;
                }
                // Gate and narrow in one step through
                // `Subscriber::delivered_mask`, which owns C's
                // twice-used `caEventMask & pevent->select`. An empty
                // posting mask means nothing changed and ands to zero,
                // so it skips there rather than needing a check here.
                if let Some(mask) = sub.delivered_mask(posting_mask) {
                    // Server-side filter chain (3.15.7). Empty chain
                    // is identity, so no behaviour change for the
                    // common no-filter case. One field's monitors share
                    // its projected log value.
                    let value: &Option<Cow<'_, EpicsValue>> = logged
                        .get_or_insert_with(|| value.as_value().map(|v| entry.declared_view(v)));
                    // C `db_queue_event_log`: append, or replace this
                    // monitor's last queued entry in place when the queue
                    // is in flow control or nearly full. The queue owns
                    // that decision and counts the displaced value.
                    let head = head.get_or_insert_with(|| self.log_head());
                    sub.post(&mut burst, head, value.as_deref(), origin, mask);
                }
            }
        }
    }

    /// Notify subscribers of a specific field, filtering by event mask.
    pub fn notify_field(&mut self, field: &str, mask: crate::server::recgbl::EventMask) {
        self.notify_field_with_origin(field, mask, 0);
    }

    /// [`Self::notify_field`] of the field declared `desc`, filed by it
    /// rather than by its name; `burst` `None` is a post that is its own.
    pub(crate) fn notify_declared(
        &mut self,
        burst: Option<&mut PostBurst>,
        desc: &'static FieldDesc,
        mask: crate::server::recgbl::EventMask,
    ) {
        self.post_put_field(
            burst,
            desc.name,
            mask,
            0,
            (
                |_: &Self| Some(desc),
                |inst: &Self| inst.resolve_field_upper_at(desc.name, Some(desc)),
            ),
            Some(desc),
        );
    }

    /// [`Self::notify_field`] posting into the caller's `burst`.
    pub fn notify_field_in(
        &mut self,
        burst: &mut PostBurst,
        field: &str,
        mask: crate::server::recgbl::EventMask,
    ) {
        self.notify_field_with_origin_in(burst, field, mask, 0);
    }

    /// C `db_post_events(precord, NULL, DBE_ALARM)`: post a record-wide
    /// alarm event. Delivers to every subscriber on any field whose mask
    /// includes DBE_ALARM, each carrying its own monitored field's current
    /// value (the per-field `notify_field` already filters by mask
    /// intersection). Used by the alarm-acknowledge (ACKT/ACKS) put path so
    /// an alarm-mask monitor on any field observes the acknowledgement.
    pub fn notify_record_alarm(&mut self) {
        self.notify_record_alarm_in(&mut PostBurst::new());
    }

    /// [`Self::notify_record_alarm`] posting into the caller's `burst`.
    pub fn notify_record_alarm_in(&mut self, burst: &mut PostBurst) {
        self.post_record_wide(burst, crate::server::recgbl::EventMask::ALARM);
    }

    /// C `db_post_events(precord, NULL, mask)`: one post of each watched
    /// field, in the order its first monitor attached, addressed by that
    /// monitor's own declaration. The fields are taken first because a post
    /// may reap closed monitors.
    fn post_record_wide(&mut self, burst: &mut PostBurst, mask: crate::server::recgbl::EventMask) {
        let mut fields: Vec<SubName> = Vec::new();
        for (at, sub) in self.subscribers.iter().enumerate() {
            if !self.subscribers[..at]
                .iter()
                .any(|first| first.watches_as(sub))
            {
                fields.push(sub.name.clone());
            }
        }
        for field in &fields {
            match field {
                SubName::Declared(desc) => self.post_put_field(
                    Some(burst),
                    desc.name,
                    mask,
                    0,
                    (
                        |_: &Self| Some(*desc),
                        |inst: &Self| inst.resolve_field_upper_at(desc.name, Some(desc)),
                    ),
                    Some(desc),
                ),
                SubName::Named(name) => self.notify_field_with_origin_in(burst, name, mask, 0),
            }
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
    pub fn notify_field_with_origin(
        &mut self,
        field: &str,
        mask: crate::server::recgbl::EventMask,
        origin: u64,
    ) {
        self.post_put_field(
            None,
            field,
            mask,
            origin,
            (
                |inst: &Self| inst.field_desc(field),
                |inst: &Self| inst.resolve_field(field),
            ),
            None,
        );
    }

    /// [`Self::notify_field_in`] posting `value`, read when the post was
    /// made, instead of the field as it stands now.
    pub(crate) fn notify_field_value_in(
        &mut self,
        burst: &mut PostBurst,
        desc: &'static FieldDesc,
        mask: crate::server::recgbl::EventMask,
        value: EpicsValue,
    ) {
        self.post_put_field(
            Some(burst),
            desc.name,
            mask,
            0,
            (|_: &Self| Some(desc), |_: &Self| Some(value)),
            Some(desc),
        );
    }

    /// Post the `db_post_events` calls a device made itself
    /// ([`crate::server::device_support::DevicePost`]), in order, as one
    /// burst. The caller holds the record, as C's caller of the device did.
    pub fn post_device_values(&mut self, posts: Vec<crate::server::device_support::DevicePost>) {
        if posts.is_empty() {
            return;
        }
        let mut burst = PostBurst::new();
        for post in posts {
            if let Some(desc) = self.field_desc(post.field) {
                self.notify_field_value_in(&mut burst, desc, post.mask, post.value);
            }
        }
    }

    /// [`Self::notify_field_with_origin`] posting into the caller's `burst`:
    /// readers wake once, when the burst drops. `put_ackt_in`/`put_acks_in`
    /// share one; no `dbPut` body passes one yet.
    pub fn notify_field_with_origin_in(
        &mut self,
        burst: &mut PostBurst,
        field: &str,
        mask: crate::server::recgbl::EventMask,
        origin: u64,
    ) {
        self.post_put_field(
            Some(burst),
            field,
            mask,
            origin,
            (
                |inst: &Self| inst.field_desc(field),
                |inst: &Self| inst.resolve_field(field),
            ),
            None,
        );
    }

    /// [`Self::notify_field_with_origin_in`] for a settled put target, into the
    /// put's `burst`; `field` is upper-case.
    pub(crate) fn notify_field_at(
        &mut self,
        burst: &mut PostBurst,
        field: &str,
        at: &PutAddr,
        mask: crate::server::recgbl::EventMask,
        origin: u64,
    ) {
        self.post_put_field(
            Some(burst),
            field,
            mask,
            origin,
            (
                |_: &Self| at.field.desc,
                |inst: &Self| inst.resolve_field_upper_at(field, at.field.desc),
            ),
            at.field.desc,
        );
    }

    /// The body of [`Self::notify_field_with_origin`], with the field's
    /// declaration and read supplied by the entry as `access`, and its
    /// declaration when the entry settled one as `declared`; a name-addressed
    /// entry resolves its key past the unwatched test.
    /// `burst` `None` is a post that is its own burst, built only past the
    /// early returns: a field no one monitors pays nothing for it.
    #[inline(always)]
    fn post_put_field(
        &mut self,
        burst: Option<&mut PostBurst>,
        field: &str,
        mask: crate::server::recgbl::EventMask,
        origin: u64,
        access: (
            impl FnOnce(&Self) -> Option<&'static FieldDesc>,
            impl FnOnce(&Self) -> Option<EpicsValue>,
        ),
        declared: Option<&'static FieldDesc>,
    ) {
        let (desc, read) = access;
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
        // Rows whose consumer went away without unsubscribing are reaped
        // from the field being posted, not by a sweep of every field per put.
        // With none left this is C `db_post_events` at `mlis.count == 0`: no
        // read, no allocation, and no published state moves; the monitor walk
        // advances that for an unsubscribed field.
        if self.unwatched() {
            return;
        }
        let key = match declared {
            Some(desc) => self.monitor_plan.key_of(desc),
            None => self.post_key(field),
        };
        if self.reap_closed(key) == 0 {
            return;
        }
        // A reference-log field is not read: its post carries no value.
        let Some(value) = self.post_payload_at(key, desc, read) else {
            return;
        };
        {
            let mut head = None;
            let mut logged = None;
            let mut own;
            let burst = match burst {
                Some(burst) => burst,
                None => {
                    own = PostBurst::new();
                    &mut own
                }
            };
            for entry in self.subscribers.iter().filter(|sub| sub.watches_key(key)) {
                let sub = &entry.sub;
                // Paused subscriber (`db_event_disable`): suppress at
                // the source — no delivery, no coalesce.
                if !sub.is_active() {
                    continue;
                }
                // Same single owner as the snapshot path: gate and
                // narrow are one operation (C `dbEvent.c:896-900`).
                if let Some(mask) = sub.delivered_mask(mask) {
                    // Server-side filter chain (3.15.7). Empty
                    // chain (the default for every subscriber
                    // until a `.{filter:opts}` PV-name suffix
                    // parser wires one in) is the identity, so
                    // existing subscribers see no behaviour
                    // change. A filter returning `None` silences
                    // this event for this subscriber only.
                    let logged: &Option<Cow<'_, EpicsValue>> = logged
                        .get_or_insert_with(|| value.as_value().map(|v| entry.declared_view(v)));
                    // Same single post owner as the snapshot path.
                    let head = head.get_or_insert_with(|| self.log_head());
                    sub.post(burst, head, logged.as_deref(), origin, mask);
                }
            }
        }
        // The value is now published: hand it to the `last_posted` owner so
        // the change detector does not publish it again. Delivery to any
        // individual subscriber may have been filtered out; the post, not the
        // delivery, is what counts.
        if publishes_value {
            self.record_value_post_as(key, field, value);
        }
    }

    /// Add a subscriber for a specific field. Refuses with
    /// [`SubscribeRefused::CapReached`] when the per-field subscriber cap (`EPICS_CAS_MAX_SUBSCRIBERS_PER_PV`)
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
    ) -> Result<EventReader, SubscribeRefused> {
        self.add_subscriber_on(
            &EventUser::new(),
            field,
            sid,
            data_type,
            mask,
            crate::server::event_queue::no_context(),
        )
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
        context: &crate::server::event_queue::DeliveryContext,
    ) -> Result<EventReader, SubscribeRefused> {
        let cap = crate::server::pv::max_subscribers_per_pv();
        // A destroyed record takes no new monitor, so `destroyed => no
        // subscribers` survives a CREATE_CHAN + EVENT_ADD that races the
        // removal. Both are `&mut self`, so there is no window between them.
        if self.destroyed {
            return Err(SubscribeRefused::NoRecord);
        }
        // Reap the field's rows whose consumer is gone before counting
        // against the cap. A record field whose value never changes (e.g. a
        // quasi-static catalog field) never triggers `post_put_field`'s reap, so a
        // long-lived subscribe-disconnect storm could pin the field at `cap`
        // worth of dead rows and lock out genuine new subscribers.
        let live = self.reap_closed(self.post_key(field));
        if live >= cap {
            return Err(SubscribeRefused::CapReached);
        }
        let desc = self.field_desc(field);
        let kind =
            crate::server::event_queue::LogKind::of_field(desc, || self.resolve_field(field));
        let (sink, reader) = crate::server::event_queue::attach_with(user, sid, kind, context);
        let subscriber = Subscriber::new(sid, data_type, mask, sink);
        let declared = declared_desc(desc, field);
        let ix = FieldIx::of(self.record.as_ref(), declared.or(desc));
        let entry = FieldSubscriber {
            name: match declared {
                Some(desc) => SubName::Declared(desc),
                None => SubName::Named(field.into()),
            },
            ix,
            declared: self.declared_field_type(field),
            long_string: self.record.field_is_long_string(field),
            walk: SubWalk::Skip,
            posted: false,
            claimed_rank: self
                .monitor_plan
                .claimed
                .iter()
                .position(|name| *name == field)
                .map_or(u16::MAX, |at| at as u16),
            roles: self.posted.roles_of(
                declared.map(|_| ix),
                self.monitor_plan.fixed_roles(self.post_key(field)),
            ),
            sub: subscriber,
        };
        // C `ellAdd`: attach order is post order — except that a claimed
        // field goes before the claimed fields the type's `monitor()` posts
        // after it, since that order, not the list's, is C's post order.
        let at = match entry.claimed_rank {
            u16::MAX => self.subscribers.len(),
            rank => self
                .subscribers
                .iter()
                .position(|sub| sub.claimed_rank != u16::MAX && sub.claimed_rank > rank)
                .unwrap_or(self.subscribers.len()),
        };
        self.subscribers.insert(at, entry);
        self.set_subscribers_watched();
        // A field with no `last_posted` entry has never been published: seed it
        // with the value the client is handed in the EVENT_ADD response, so the
        // first process cycle does not treat it as changed. An existing entry
        // is left alone — every post advances it whether or not anyone was
        // subscribed, so it is what was last published, and a change since
        // then is still owed to the field's other subscribers.
        if self.posted_value(field).is_none() {
            let seed = if kind == crate::server::event_queue::LogKind::Ref {
                Some(PostedValue::Ref)
            } else {
                self.resolve_field(field).map(PostedValue::Val)
            };
            if let Some(seed) = seed {
                self.record_value_post(field, seed);
            }
        }
        self.settle_sub_walk();
        Ok(reader)
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
        match self
            .subscribers
            .iter_mut()
            .rev()
            .find(|sub| sub.watches_str(field))
        {
            Some(entry) => {
                entry.sub.push_filter(filter);
                true
            }
            None => false,
        }
    }

    /// C `db_post_single_event` for the `field` subscriber holding slot `id`,
    /// queueing a snapshot the caller read; outside this crate the read and
    /// the queue go through
    /// [`channel_io::post_single_event`](crate::server::database::channel_io::post_single_event),
    /// which keeps them in one hold.
    pub(crate) fn post_single_event(
        &self,
        field: &str,
        id: crate::server::event_queue::AttachId,
        snapshot: Arc<crate::server::snapshot::Snapshot>,
    ) {
        if let Some(entry) = self
            .subscribers
            .iter()
            .find(|sub| sub.sub.id() == id && sub.watches_str(field))
        {
            entry.sub.post_single(snapshot);
        }
    }

    /// Remove the subscriber holding slot `id` (its reader's
    /// [`EventReader::id`](crate::server::event_queue::EventReader::id)).
    pub fn remove_subscriber(&mut self, id: crate::server::event_queue::AttachId) {
        self.retain_subscribers(|sub| sub.sub.id() != id);
    }

    /// Whether any monitor watches `field`.
    pub fn has_subscriber(&self, field: &str) -> bool {
        self.subscribers.iter().any(|sub| sub.watches_str(field))
    }

    /// How many monitors watch `field`.
    pub fn subscriber_count(&self, field: &str) -> usize {
        self.subscribers
            .iter()
            .filter(|sub| sub.watches_str(field))
            .count()
    }

    /// `field`'s monitors, in attach order.
    pub fn subscribers_of<'a>(&'a self, field: &'a str) -> impl Iterator<Item = &'a Subscriber> {
        self.subscribers
            .iter()
            .filter(move |sub| sub.watches_str(field))
            .map(|sub| &sub.sub)
    }

    /// Every monitor on the record, in attach order.
    pub fn field_subscribers(&self) -> &[FieldSubscriber] {
        &self.subscribers
    }

    /// Destroy this record: drop every field monitor and refuse every future
    /// one, drop every link target handle it holds, and drop every queued
    /// put-notify and end the active one, whose clients then see their
    /// completion dropped. The cell
    /// itself is kept by the record table, so these releases happen here or
    /// not at all. The record-backed
    /// half of the rule [`crate::server::pv::ProcessVariable::destroy`]
    /// states for simple PVs, so one sweep in a server closes both kinds of
    /// channel; and the one place a record lets go of the records its links
    /// resolved to, so a destroyed record keeps no other alive. Returns
    /// `true` for the call that performed the transition.
    pub(crate) fn destroy(&mut self) -> bool {
        let first = !self.destroyed;
        self.destroyed = true;
        self.subscribers.clear();
        self.set_subscribers_watched();
        self.notify_restart_list.clear();
        if let Some(ws) = self.notify.take() {
            ws.abandon();
        }
        for entry in self.parsed_inputs.iter_mut().flatten() {
            entry.release_target();
        }
        self.release_link_addrs();
        first
    }

    /// Drop every link target handle that names `cell` — the other half of
    /// the rule [`Self::destroy`] keeps: a handle never outlives its
    /// target's map entry. The remover calls it on every record left in the
    /// map once the entry is gone, so a record that never processes again
    /// does not keep the removed one alive.
    pub(crate) fn release_link_targets_to(&mut self, cell: &Arc<RecordCell>) {
        for entry in self.parsed_inputs.iter_mut().flatten() {
            if entry.targets(cell) {
                entry.release_target();
            }
        }
        // Every handle the cells hold, not only `cell`'s: the removal moved
        // the name revision, so the next access re-resolves anyway.
        self.release_link_addrs();
    }

    /// Empty the address cell of every link this record lends a target
    /// through (`DbLink::release_addr`): INP for its reads, OUT and the
    /// type's declared links for its writes.
    fn release_link_addrs(&mut self) {
        // Before the entries go: the targets borrow them.
        self.parsed_out.set_out_target(None);
        let links = [self.parsed_inp.link(), self.parsed_out.link()]
            .into_iter()
            .chain(self.link_slots.links().map(|l| &**l));
        for link in links {
            if let ParsedLink::Db(db) = link {
                // SAFETY: `&mut self` excludes `PvDatabase::inp_addr`'s
                // borrow, and the acquire that gave it excludes every
                // `DbLink::lend_out_addr` holder of this record's set.
                unsafe { db.release_addr() }
            }
        }
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
    /// never reaches into the event queue. No-op if no subscriber holds slot
    /// `id`. Takes `&self`: the flag is atomic in the shared entry, so a post
    /// reading the same entry sees the change with no list rebuilt.
    pub fn set_subscriber_active(&self, id: crate::server::event_queue::AttachId, active: bool) {
        for entry in self.subscribers.iter() {
            if entry.sub.id() == id {
                entry.sub.set_active(active);
            }
        }
    }

    /// Clean up subscriber rows whose consumer is gone.
    pub fn cleanup_subscribers(&mut self) {
        self.retain_subscribers(|sub| !sub.sub.is_closed());
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
pub(crate) fn check_deadband(newval: f64, oldval: f64, deadband: f64) -> bool {
    // A finite difference means both operands were finite (inf - x is inf,
    // inf - inf and NaN - x are NaN), so the common case is C's own
    // subtract, fabs, compare.
    let delta = (oldval - newval).abs();
    if delta.is_finite() {
        return delta > deadband;
    }
    let delta = if newval.is_finite() && oldval.is_finite() {
        // The subtraction overflowed.
        f64::INFINITY
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
mod record_cell_layout_tests {
    /// The test build is the other side of the boundary: its `#[cfg(test)]`
    /// fields change `RecordInstance`'s size, and under rustc's own field order
    /// that alone put `data` at 0xb8 here while the release build had it at 0.
    #[test]
    fn data_stays_first_when_the_instance_changes_size() {
        assert_eq!(std::mem::offset_of!(super::RecordCell, data), 0);
    }

    use super::{CommonFields, RecordCell, RecordInstance};

    fn size_of_field<T, F>(_: fn(&T) -> &F) -> usize {
        std::mem::size_of::<F>()
    }

    fn align_of_field<T, F>(_: fn(&T) -> &F) -> usize {
        std::mem::align_of::<F>()
    }

    /// `$t`'s fields in declared order, as (name, offset, size, align). The
    /// pattern has no `..`, so a field added to `$t` and not listed here
    /// breaks the build.
    macro_rules! layout {
        ($t:ident { $($f:ident),+ $(,)? }) => {{
            let _ = |v: &$t| {
                let $t { $($f: _),+ } = v;
            };
            let fields = [$((
                stringify!($f),
                std::mem::offset_of!($t, $f),
                size_of_field(|v: &$t| &v.$f),
                align_of_field(|v: &$t| &v.$f),
            )),+];
            (stringify!($t), std::mem::size_of::<$t>(), std::mem::align_of::<$t>(), fields)
        }};
    }

    /// The fields sit at their listed positions with nothing between them but
    /// the padding the next one's alignment needs.
    fn packed_in_order<const N: usize>(
        (ty, size, align, fields): (&str, usize, usize, [(&str, usize, usize, usize); N]),
    ) {
        let mut end = 0;
        for (f, at, fsize, falign) in fields {
            assert!(
                at >= end && at - end < falign,
                "{ty}.{f} at {at} does not follow the field before it (ending at {end})"
            );
            end = at + fsize;
        }
        assert!(
            size - end < align,
            "{ty} has {} bytes after its last field",
            size - end
        );
    }

    /// The fields an unmonitored cycle reads, in the order it reads them,
    /// fill one run: CommonFields' tail, inside RecordInstance's tail, which
    /// RecordCell's own read fields continue. Every field of the three types
    /// is listed in declared order, the cold ones on the far side of the run;
    /// a field added, moved or planted in the run fails the build or here.
    #[test]
    fn the_cycle_read_fields_stay_one_run() {
        packed_in_order(layout!(CommonFields {
            text,
            bkpt,
            utag,
            hyst,
            scan,
            sscn,
            acks,
            udfs,
            oldsimm,
            pini,
            phas,
            prio,
            ackt,
            asl,
            disp,
            proc_field,
            // Read every process cycle from here on.
            disv,
            disa,
            diss,
            lcnt,
            nsev,
            nsta,
            sevr,
            stat,
            stamp,
            putf,
            tpro,
            rpro,
            udf,
            dtyp,
            time,
            time_last,
            analog_alarm,
            namsg,
            amsg,
            mlst,
            alst,
        }));
        let instance = layout!(RecordInstance {
            parsed_inputs,
            link_slots,
            link_rows,
            device,
            device_done,
            dpvt,
            subroutine,
            reprocess_generation,
            watchdog_generation,
            cold,
            plan_forced_general,
            ran_arm,
            cycle_links,
            selm,
            dset_init,
            simpvt,
            suppress_subroutine_run,
            // Read every process cycle from here on.
            subscribers,
            common,
            record,
            notify,
            notify_restart_list,
            name,
            udf_alarm,
            cycle_plan,
            plan_rev,
            simm_before_siml_read,
            dol_mode,
            destroyed,
            pact,
            array_hash_changed,
            parsed_sdis,
            parsed_inp,
            parsed_out,
            parsed_tsel,
            parsed_flnk,
            monitor_hot,
            last_posted,
            posted,
            monitor_plan,
        });
        packed_in_order(instance);
        let cell = layout!(RecordCell {
            data,
            lock_record,
            borrow,
            cvt_pending,
            plan,
            name,
            // Not read by a process cycle from here on.
            init_owed,
            link_opens_owed,
            chain_pact,
            index,
            rdes,
            slots,
            put_facts,
            multi_input_val_slots,
            load_order,
            bkpt,
            string_typed,
            dbaddr_counts,
            metadata,
        });
        packed_in_order(cell);

        // The cell prefetch covers this run and nothing past it: from the
        // first field after the instance's marker to the last before the
        // cell's.
        fn field(fields: &[(&str, usize, usize, usize)], name: &str) -> (usize, usize) {
            let f = fields.iter().find(|f| f.0 == name).unwrap();
            (f.1, f.2)
        }
        let (first, _) = field(&instance.3, "subscribers");
        let (last, last_size) = field(&cell.3, "name");
        assert_eq!(RecordCell::CYCLE_RUN, first..last + last_size);
        assert!(
            instance
                .3
                .iter()
                .take_while(|f| f.0 != "subscribers")
                .all(|f| f.1 + f.2 <= first),
            "a field before the run's first sits inside it"
        );
        assert!(
            cell.3
                .iter()
                .skip_while(|f| f.0 != "init_owed")
                .all(|f| f.1 >= last + last_size),
            "a field after the run's last sits inside it"
        );
    }
}

#[cfg(test)]
mod device_menu_marking_tests {
    use super::*;
    use crate::server::records::ai::AiRecord;
    use crate::server::records::calc::CalcRecord;
    use crate::server::records::mbbo::MbboRecord;

    const fn test_row(choice: &'static str) -> crate::server::record::DevSup {
        crate::server::record::DevSup {
            name: choice,
            choice,
            link_type: crate::server::record::DbLinkType::Constant,
            dset: crate::server::record::Dset::new(5, &[], false),
        }
    }

    /// C `dbAccess.c:176-179`: a `DBF_DEVICE` field whose record type declares
    /// no device support has `pfldDes->ftPvt == NULL` and takes `goto nostrs`,
    /// which clears `DBR_ENUM_STRS` — the client is sent NO choice list.
    ///
    /// `calc` declares no `device()` line, so QSRV2 omits `value.choices` on
    /// `CALC.DTYP`. The port used to default the missing menu to `[]` and mark
    /// an empty list instead.
    #[test]
    fn dtyp_of_a_record_type_with_no_device_support_supplies_no_choices() {
        let inst = RecordInstance::new("X", CalcRecord::default());
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
        let inst = RecordInstance::new("X", AiRecord::default());
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
        let inst = RecordInstance::new("X", CalcRecord::default());
        assert_eq!(inst.dtyp_index(), 0);
    }

    /// A DTYP put runs under the record's hold, so a menu choice must land as
    /// the menu's own pointer, whether it arrives as the index or the label.
    #[test]
    fn a_dtyp_put_stores_the_device_menu_choice() {
        use crate::server::device_support::DtypName;
        let mut inst = RecordInstance::new("X", AiRecord::default());
        let raw = super::super::device_menu_choices("ai")
            .position(|c| c == "Raw Soft Channel")
            .unwrap();
        for value in [
            EpicsValue::Enum(raw as u16),
            EpicsValue::String("Raw Soft Channel".into()),
        ] {
            inst.common.dtyp.set_name("Soft Channel");
            inst.put_common_field_prepared("DTYP", value, &mut None)
                .unwrap();
            assert!(matches!(
                inst.common.dtyp.shared_name(),
                DtypName::Menu("Raw Soft Channel")
            ));
        }
    }

    #[test]
    fn a_record_without_its_dset_is_still_bound_iocinit_529() {
        let mut inst = RecordInstance::new("X", AiRecord::default());
        inst.common.dtyp = "asynInt32".into();
        assert!(!inst.init_record_reaches_body());
        inst.run_init_pass0("X");
        inst.common.dtyp.set_name("Soft Channel");
        assert_eq!(inst.common.dtyp.soft(), None);
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
        static ASYN_MBBO: &[crate::server::record::DevSup] =
            &[test_row("asynInt32"), test_row("asynUInt32Digital")];
        super::super::register_device_menu("mbbo", ASYN_MBBO);

        let mut inst = RecordInstance::new("X", MbboRecord::default());
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
        static ASYN_MBBO: &[crate::server::record::DevSup] =
            &[test_row("asynInt32"), test_row("asynUInt32Digital")];
        super::super::register_device_menu("mbbo", ASYN_MBBO);

        let inst = RecordInstance::new("X", CalcRecord::default());
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
        RecordInstance::new("TEMP", rec)
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
            inst.notify_field_with_origin("VAL", EventMask::VALUE, 0);
        }
        let after = dropped_monitor_events();
        assert!(
            after > before,
            "a post that replaces an unobserved queued entry must record a \
             dropped monitor event (before={before}, after={after})"
        );
    }

    /// One put's posts of a by-reference field share its burst: the parked
    /// reader is woken once, at the burst's drop, and finds one entry — C's
    /// duplicate drop (`dbEvent.c:794-800`) — where a wake per post let it
    /// take the first post before the second appended.
    #[test]
    fn posts_into_one_burst_wake_once_and_collapse() {
        use crate::server::recgbl::EventMask;
        use std::future::Future;
        use std::sync::atomic::AtomicUsize;
        use std::task::{Context, Wake, Waker};
        struct Count(AtomicUsize);
        impl Wake for Count {
            fn wake(self: Arc<Self>) {
                self.0.fetch_add(1, Ordering::Relaxed);
            }
        }
        let mut inst = ai_instance();
        let mut reader = inst
            .add_subscriber(
                "DESC",
                1,
                crate::types::DbFieldType::String,
                EventMask::VALUE.bits(),
            )
            .expect("subscriber added");
        let wakes = Arc::new(Count(AtomicUsize::new(0)));
        let waker = Waker::from(wakes.clone());
        assert!(
            Box::pin(reader.recv())
                .as_mut()
                .poll(&mut Context::from_waker(&waker))
                .is_pending()
        );

        let mut burst = PostBurst::new();
        let desc = inst.field_desc("DESC").unwrap();
        inst.notify_declared(Some(&mut burst), desc, EventMask::VALUE);
        inst.notify_declared(Some(&mut burst), desc, EventMask::VALUE);
        assert_eq!(wakes.0.load(Ordering::Relaxed), 0, "no wake mid-burst");
        drop(burst);
        assert_eq!(wakes.0.load(Ordering::Relaxed), 1);
        assert!(reader.try_recv().is_ok());
        assert!(reader.try_recv().is_err(), "the second post collapsed");
    }

    /// A row whose reader dropped without unsubscribing is reaped by the
    /// next post to its field; the live row beside it stays.
    #[test]
    fn a_post_reaps_its_fields_closed_subscriber_rows() {
        use crate::server::recgbl::EventMask;
        let mut inst = ai_instance();
        let mut add = |sid| {
            inst.add_subscriber(
                "VAL",
                sid,
                crate::types::DbFieldType::Double,
                EventMask::VALUE.bits(),
            )
            .expect("subscriber added")
        };
        let gone = add(1);
        let _live = add(2);
        drop(gone);
        inst.notify_field_with_origin("VAL", EventMask::VALUE, 0);
        let sids: Vec<u32> = inst.subscribers_of("VAL").map(|s| s.sid).collect();
        assert_eq!(sids, [2]);
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

        // Every cell `explicit_alarm_limits` reads, now that the cache holds
        // its answer: the four bands and the four severities that gate them.
        assert!(is_metadata_cache_source("HIHI"));
        assert!(is_metadata_cache_source("HIGH"));
        assert!(is_metadata_cache_source("LOW"));
        assert!(is_metadata_cache_source("LOLO"));
        assert!(is_metadata_cache_source("HHSV"));
        assert!(is_metadata_cache_source("HSV"));
        assert!(is_metadata_cache_source("LSV"));
        assert!(is_metadata_cache_source("LLSV"));

        assert!(!is_metadata_cache_source("VAL"));
        assert!(!is_metadata_cache_source("DESC"));
        assert!(!is_metadata_cache_source("SCAN"));
        assert!(!is_metadata_cache_source("PHAS"));
    }

    #[test]
    fn cache_starts_empty_then_populates_on_first_snapshot() {
        let inst = ai_instance();

        // Cache starts empty
        assert!(inst.cold.metadata_cache.load().is_none());

        // First snapshot triggers populate + cache store
        let snap = inst.snapshot_for_field("VAL").unwrap();
        let display = snap.display.expect("ai snapshot must have display");
        assert_eq!(display.units, "degC");
        assert_eq!(display.precision, 2);
        assert_eq!(display.upper_disp_limit, 100.0);
        assert_eq!(display.lower_disp_limit, 0.0);

        // Cache is now populated
        assert!(inst.cold.metadata_cache.load().is_some());
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

    /// The GET door serves the raw stamp and utag whatever `Q:time:tag`
    /// says, as rsrv's `dbGetField` does; the split is pvxs's alone
    /// (`iocsource.cpp:239-248`), made by each PVA reader.
    #[test]
    fn qtime_tag_get_door_serves_the_raw_stamp() {
        let mut inst = ai_instance();
        inst.common.time = crate::types::WallTime::from_unix(42, 123_456_700);
        inst.common.utag = 5;
        inst.set_info("Q:time:tag", "nsec:lsb:31");

        let snap = inst.snapshot_for_field("VAL").unwrap();
        assert_eq!(snap.user_tag, 5);
        assert_eq!(snap.timestamp.subsec_nanos(), 123_456_700);
        assert_eq!(snap.timestamp.unix_secs(), 42);
    }

    /// A monitor post carries the raw stamp and utag whatever `Q:time:tag`
    /// says, as `db_create_field_log` copies `prec->time` (`dbEvent.c:710`);
    /// the split is the PVA consumer's, from the mask its subscription read.
    #[test]
    fn qtime_nsec_mask_leaves_the_monitor_post_raw() {
        let mut inst = ai_instance();
        inst.common.time = crate::types::WallTime::from_unix(42, 123_456_700);
        inst.common.utag = 5;
        inst.set_info("Q:time:tag", "nsec:lsb:31");

        let mon = inst.log_head();
        assert_eq!(mon.utag, 5);
        assert_eq!(mon.time.subsec_nanos(), 123_456_700);
        assert_eq!(mon.time.unix_secs(), 42);
    }

    /// The mirror boundary: a tag pvxs's `strncmp` rejects must leave the
    /// timestamp and the record's own utag alone. The old case-insensitive
    /// split matched `NSEC:LSB:4` and masked the wire timestamp pvxs serves
    /// unmasked.
    #[test]
    fn qtime_uppercase_tag_leaves_timestamp_untouched() {
        let mut inst = ai_instance();
        inst.common.time = crate::types::WallTime::from_unix(42, 123_456_700);
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
    /// and MONITOR (a post's `log_head`) paths. Pre-fix both hard-set
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

        assert_eq!(
            inst.log_head().utag,
            want,
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
    fn refresh_rebuilds_a_built_cache() {
        let inst = ai_instance();
        let _ = inst.snapshot_for_field("VAL");
        let built = builds();

        inst.refresh_metadata_cache();
        assert_eq!(builds(), built + 1);
        assert!(inst.cold.metadata_cache.load().is_some());
    }

    #[test]
    fn refresh_leaves_an_unbuilt_cache_unbuilt() {
        let inst = ai_instance();
        let built = builds();
        inst.refresh_metadata_cache();
        assert_eq!(builds(), built);
        assert!(inst.cold.metadata_cache.load().is_none());
    }

    #[test]
    fn notify_field_written_invalidates_for_metadata_field() {
        let inst = ai_instance();
        let _ = inst.snapshot_for_field("VAL");
        assert!(inst.cold.metadata_cache.load().is_some());

        let built = builds();
        // Writing a metadata field rebuilds it
        inst.notify_field_written("EGU");
        assert_eq!(builds(), built + 1);
    }

    #[test]
    fn notify_field_written_skips_non_metadata_field() {
        let inst = ai_instance();
        let _ = inst.snapshot_for_field("VAL");
        assert!(inst.cold.metadata_cache.load().is_some());

        let built = builds();
        // Writing a value field does NOT rebuild the cache
        inst.notify_field_written("VAL");
        assert_eq!(builds(), built);

        // DESC is not property-class either — its cache invalidation
        // is owned by the DESC arm of `put_common_field`, not by this
        // notify path (UI-106).
        inst.notify_field_written("DESC");
        assert_eq!(builds(), built);
    }

    #[test]
    fn notify_field_written_is_case_insensitive() {
        let inst = ai_instance();
        let _ = inst.snapshot_for_field("VAL");
        assert!(inst.cold.metadata_cache.load().is_some());

        let built = builds();
        // Lowercase metadata field name still triggers the rebuild
        inst.notify_field_written("egu");
        assert_eq!(builds(), built + 1);
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
        assert!(inst.cold.metadata_cache.load().is_some());

        // Capture prev, do a no-op put, then notify — cache must remain.
        let built = builds();
        let prev = inst.record.get_field("EGU");
        let _ = inst.record.put_field("EGU", prev.clone().unwrap());
        inst.notify_field_written_if_changed("EGU", prev.as_ref());
        assert_eq!(
            builds(),
            built,
            "no-op put must not rebuild the metadata cache"
        );
    }

    /// And when the value DID change, the cache must invalidate.
    #[test]
    fn notify_field_written_if_changed_invalidates_on_real_change() {
        let mut inst = ai_instance();
        let _ = inst.snapshot_for_field("VAL");
        assert!(inst.cold.metadata_cache.load().is_some());

        let prev = inst.record.get_field("EGU");
        let _ = inst
            .record
            .put_field("EGU", EpicsValue::String("kPa".into()));
        inst.notify_field_written_if_changed("EGU", prev.as_ref());
        let cached = inst.cold.metadata_cache.load_full().expect("still built");
        assert_eq!(
            cached.display.as_ref().unwrap().units,
            "kPa",
            "real metadata change must rebuild the cache"
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
        let built = builds();
        inst.put_common_field("DESC", EpicsValue::String("after".into()))
            .unwrap();
        assert_eq!(
            builds(),
            built + 1,
            "a changed DESC must rebuild the metadata cache"
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
        let built = builds();
        inst.put_common_field("DESC", EpicsValue::String("same".into()))
            .unwrap();
        assert_eq!(
            builds(),
            built,
            "an unchanged DESC must not rebuild the metadata cache"
        );
    }

    /// Non-metadata fields don't carry property semantics — the
    /// `if_changed` variant must never invalidate for them, matching
    /// the existing `notify_field_written` short-circuit.
    #[test]
    fn notify_field_written_if_changed_skips_non_metadata_field() {
        let mut inst = ai_instance();
        let _ = inst.snapshot_for_field("VAL");
        assert!(inst.cold.metadata_cache.load().is_some());
        // VAL is neither a cache source nor `prop(YES)` — must be skipped
        // even with a changed value.
        let built = builds();
        inst.notify_field_written_if_changed("VAL", None);
        assert_eq!(builds(), built);
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
        let lo = RecordInstance::new("LO", LongoutRecord::default());
        let lo = lo.snapshot_for_field("VAL").unwrap();
        assert!(!lo.properties.precision && lo.precision().is_none());
        assert!(lo.properties.units && lo.properties.graphic_double);

        // stringout: no property slot at all.
        let so = RecordInstance::new("SO", StringoutRecord::default());
        let so = so.snapshot_for_field("VAL").unwrap();
        assert_eq!(so.properties, PropertySupport::NONE);
        assert!(so.units().is_none(), "a stringout supplies no EGU");

        // waveform: `#define get_alarm_double NULL`.
        let wf = RecordInstance::new("WF", WaveformRecord::default());
        let wf = wf.snapshot_for_field("VAL").unwrap();
        assert!(
            !wf.properties.alarm_double && wf.alarm_limits().is_none(),
            "a waveform supplies no alarm limits — a GUI must not draw bands at zero"
        );
        assert!(wf.properties.units && wf.properties.graphic_double);
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
        let inst = RecordInstance::new("PFM", PerFieldMetaRecord);

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
        fn monitor_deadband_value(&self) -> Option<crate::server::record::AlarmLimit> {
            Some(crate::server::record::AlarmLimit::Double(self.rbv))
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
            "RDB",
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
        let names =
            |snap: &ProcessSnapshot| snap.iter().map(|(n, _, _)| n.clone()).collect::<Vec<_>>();

        // Cycle 1 (first publish): RBV fires via the deadband trigger
        // (MLST starts at the NaN never-posted sentinel). VAL must NOT
        // post: `add_subscriber` seeded `last_posted` with the current
        // value (the initial value already went out with EVENT_ADD), and
        // C monitor() posts VAL only when MARKED(M_VAL) — nothing marked
        // it.
        let (snap, _) = inst.process_local().unwrap();
        let n = names(&snap);
        assert!(n.iter().any(|f| f == "RBV"), "{n:?}");
        assert!(
            !n.iter().any(|f| f == "VAL"),
            "VAL unchanged since subscribe must not post: {n:?}"
        );

        // Cycle 2: RBV moved past MDEL, VAL unchanged → RBV posted,
        // VAL not re-posted.
        let (snap, _) = inst.process_local().unwrap();
        let n = names(&snap);
        assert!(n.iter().any(|f| f == "RBV"), "RBV crossed MDEL: {n:?}");
        assert!(
            !n.iter().any(|f| f == "VAL"),
            "unchanged VAL must not post: {n:?}"
        );

        // Cycle 3: widen the deadband — RBV moves within it → throttled.
        let _ = inst.record.put_field("MDEL", EpicsValue::Double(1000.0));
        let (snap, _) = inst.process_local().unwrap();
        let n = names(&snap);
        assert!(
            !n.iter().any(|f| f == "RBV"),
            "MDEL must throttle RBV: {n:?}"
        );

        // Cycle 4: setpoint moves while RBV stays inside the deadband →
        // VAL posts via change detection, RBV stays throttled.
        let _ = inst.record.put_field("VAL", EpicsValue::Double(42.0));
        let (snap, _) = inst.process_local().unwrap();
        let n = names(&snap);
        assert!(n.iter().any(|f| f == "VAL"), "changed VAL must post: {n:?}");
        assert!(
            !n.iter().any(|f| f == "RBV"),
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

        let mut inst = RecordInstance::new("ASUB", ASubRecord::default());
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
        let posts_val = |snap: &ProcessSnapshot| snap.iter().any(|(n, _, _)| n == "VAL");

        // A settling scan of the unchanged record: C `do_sub` returns 0 and
        // `process` leaves VAL at 0 (already 0), settling the monitor gate.
        let _ = inst.process_local().unwrap();
        assert_eq!(inst.record.get_field("VAL"), Some(EpicsValue::Long(0)));
        // VAL is the status; with no OUTx wired there is no pair to drive.
        assert!(inst.record.multi_output_links().is_empty());

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
        fn force_posted_fields(&self) -> crate::server::record::DeclaredFields {
            &const { crate::server::record::declared_set(FORCE_POST_FIELDS, ["DIFF"]) }
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
            "FP",
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
        let names =
            |snap: &ProcessSnapshot| snap.iter().map(|(n, _, _)| n.clone()).collect::<Vec<_>>();

        // Cycle 1 (first publish): both DIFF and VAL post — last_posted is
        // empty so change-detection treats every subscribed field as new.
        let (snap1, _) = inst.process_local().unwrap();
        assert!(
            names(&snap1).iter().any(|f| f == "DIFF"),
            "DIFF posts on first publish: {:?}",
            names(&snap1)
        );

        // Cycle 2: nothing changed. VAL (not force-listed) must NOT re-post;
        // DIFF (force-listed) MUST re-post — the C unconditional MARK +
        // DBE_VAL_LOG. This is the divergence MOT-1 closes.
        let (snap2, _) = inst.process_local().unwrap();
        assert!(
            names(&snap2).iter().any(|f| f == "DIFF"),
            "force-posted DIFF must re-post when unchanged: {:?}",
            names(&snap2)
        );
        assert!(
            !names(&snap2).iter().any(|f| f == "VAL"),
            "an unchanged non-force field must not re-post: {:?}",
            names(&snap2)
        );
        // The forced re-post carries DBE_VALUE|DBE_LOG (no alarm bits this
        // cycle), matching C `monitor_mask | DBE_VAL_LOG` with monitor_mask=0.
        let diff_mask = snap2
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
        fn log_swept_fields(&self) -> crate::server::record::DeclaredFields {
            &const { crate::server::record::declared_set(LOG_SWEEP_FIELDS, ["S1"]) }
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
        let mut inst = RecordInstance::new("SW", LogSweepRecord { s1: 7, s2: 9 });
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
        let names =
            |snap: &ProcessSnapshot| snap.iter().map(|(n, _, _)| n.clone()).collect::<Vec<_>>();
        let count_of =
            |snap: &ProcessSnapshot, f: &str| snap.iter().filter(|(n, _, _)| n == f).count();
        let mask_of = |snap: &ProcessSnapshot, f: &str| {
            snap.iter().find(|(n, _, _)| n == f).map(|(_, _, m)| *m)
        };

        // Cycle 1: nothing changed since subscribe. S1 (swept) re-posts
        // with DBE_LOG ONLY; S2 (not swept) must NOT re-post.
        let (snap1, _) = inst.process_local().unwrap();
        assert!(
            names(&snap1).iter().any(|f| f == "S1"),
            "log-swept S1 must re-post when unchanged: {:?}",
            names(&snap1)
        );
        assert!(
            !names(&snap1).iter().any(|f| f == "S2"),
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
            snap2.iter().collect::<Vec<_>>()
        );
        let s1_masks: Vec<u16> = snap2
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
        fn log_swept_fields(&self) -> crate::server::record::DeclaredFields {
            &const { crate::server::record::declared_set(ALARMING_LOG_SWEEP_FIELDS, ["S1"]) }
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
            "SW",
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
            snap.iter().find(|(n, _, _)| n == f).map(|(_, _, m)| *m)
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
            "MUT",
            MutatingMetaRecord {
                val: 1.0,
                egu: "V".to_string(),
                took_change: false,
            },
        );

        // Build the cache once with the original EGU
        let snap1 = inst.snapshot_for_field("VAL").unwrap();
        assert_eq!(snap1.display.unwrap().units, "V");
        assert!(inst.cold.metadata_cache.load().is_some());

        // Run process_local — the stub record sets took_change inside process()
        let _ = inst.process_local();

        // Rebuilt from the new EGU (took_metadata_change returned true)
        let cached = inst.cold.metadata_cache.load_full().expect("still built");
        assert_eq!(
            cached.display.as_ref().unwrap().units,
            "kV",
            "process_local should rebuild the cache when took_metadata_change is true"
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
        let mut inst = RecordInstance::new("STABLE", StableMetaRecord { val: 0.0 });

        let _ = inst.snapshot_for_field("VAL");
        let built = builds();

        // Run process_local several times — cache should remain intact
        let _ = inst.process_local();
        let _ = inst.process_local();
        let _ = inst.process_local();
        assert_eq!(builds(), built);
    }

    // ── Regression: DBE_PROPERTY event delivery boundaries ──────────────

    /// Subscribe `VAL` for PROPERTY, put `field`, run the post gate, and report
    /// whether an event was delivered. `put` must differ from the field's
    /// current value or the change-detection suppresses the post either way.
    fn property_event_on_put<R: Record>(rec: R, field: &str, put: EpicsValue) -> bool {
        use crate::server::recgbl::EventMask;
        let mut inst = RecordInstance::new("PROPGATE", rec);
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
        inst.notify_field_written_if_changed(field, prev.as_ref());
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
        inst.notify_field_written_if_changed("EGU", prev.as_ref());

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
        inst.notify_field_written_if_changed("EGU", prev.as_ref());

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
        inst.notify_field_written_if_changed("EGU", prev.as_ref());

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
            "MUT2",
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

    fn put_siol(inst: &mut RecordInstance, text: &str) {
        inst.put_merged_field("SIOL", EpicsValue::String(text.into()))
            .expect("SIOL put");
    }

    fn db_link(parsed: &ParsedLink) -> &crate::server::record::DbLink {
        match parsed {
            ParsedLink::Db(db) => db,
            other => panic!("expected a Db link, got {other:?}"),
        }
    }

    /// SIOL is read from its slot: the parse each put installs, the same
    /// `Arc` between puts.
    #[test]
    fn sim_link_reads_the_parse_each_put_installs() {
        let mut inst = ai_instance();
        put_siol(&mut inst, "SRC.VAL");
        let first = inst.sim_link(SimLink::Siol).clone();
        assert!(Arc::ptr_eq(&first, inst.sim_link(SimLink::Siol)));
        assert_eq!(db_link(&first).pvname(), "SRC");
        put_siol(&mut inst, "OTHER.VAL");
        assert_eq!(db_link(inst.sim_link(SimLink::Siol)).pvname(), "OTHER");
    }

    /// SIOL's CP survives as `DBF_INLINK` (ai) and is discarded as
    /// `DBF_OUTLINK` (ao).
    #[test]
    fn sim_link_modifier_mask_follows_the_field_type() {
        let mut input = ai_instance();
        put_siol(&mut input, "OTHER.VAL CP");
        match input.sim_link(SimLink::Siol).as_ref() {
            ParsedLink::Ca(ca) => assert_eq!(
                ca.policy,
                crate::server::record::LinkProcessPolicy::ChannelProcess
            ),
            other => panic!("a CP SIOL is a CA link, got {other:?}"),
        }
        let mut output = RecordInstance::new("AO", crate::server::records::ao::AoRecord::default());
        put_siol(&mut output, "OTHER.VAL CP");
        assert_eq!(
            db_link(output.sim_link(SimLink::Siol)).policy,
            crate::server::record::LinkProcessPolicy::NoProcess
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
    use std::time::Duration;

    fn at(secs: f64) -> crate::types::WallTime {
        crate::types::WallTime::UNIX_EPOCH + Duration::from_secs_f64(secs)
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
                    check_deadband(*newval, *oldval, deadband),
                    want,
                    "C pattern {i}: deadband={deadband} oldval={oldval} newval={newval}"
                );
            }
        }
    }

    const MAX: f64 = f64::MAX;

    /// The boundaries C's table leaves out, against deadbands it never uses.
    /// Every expectation is C's `delta > deadband` with `delta` from
    /// recGbl.c:348-363: finite operands give `fabs(old - new)`, which is
    /// `inf` when the subtraction overflows; a NaN/inf mismatch or opposite
    /// infinities give `epicsINF`; both NaN and same-signed infinities keep
    /// `delta = 0`. A NaN deadband fails every compare, an `inf` one fails
    /// every compare including `inf > inf`, and `-inf` passes even `0`.
    #[test]
    fn matches_c_at_the_non_finite_and_overflow_boundaries() {
        // [oldval, newval] and C's delta for the pair.
        let pairs: [(f64, f64, f64); 13] = [
            (MAX, -MAX, INF), // finite operands, the subtraction overflows
            (-MAX, MAX, INF),
            (1.0, NAN, INF),
            (NAN, 1.0, INF),
            (1.0, INF, INF),
            (INF, 1.0, INF),
            (1.0, -INF, INF),
            (-INF, 1.0, INF),
            (INF, -INF, INF),
            (-INF, INF, INF),
            (NAN, NAN, 0.0),
            (INF, INF, 0.0),
            (1.0, 2.0, 1.0),
        ];
        // deadband, then the expected update per pair.
        let expected: [(f64, [bool; 13]); 5] = [
            (
                0.0,
                [
                    true, true, true, true, true, true, true, true, true, true, false, false, true,
                ],
            ),
            (
                MAX,
                [
                    true, true, true, true, true, true, true, true, true, true, false, false, false,
                ],
            ),
            (INF, [false; 13]),
            (NAN, [false; 13]),
            (-INF, [true; 13]),
        ];

        for (deadband, row) in expected {
            for ((oldval, newval, delta), want) in pairs.iter().zip(row) {
                assert_eq!(
                    check_deadband(*newval, *oldval, deadband),
                    want,
                    "deadband={deadband} oldval={oldval} newval={newval} (C delta={delta})"
                );
            }
        }
    }

    /// Deadband 0 posts any change at all: C's `delta > 0` holds for one ulp
    /// and for the smallest subnormal, and fails only for an equal value.
    #[test]
    fn zero_deadband_posts_one_ulp_but_not_an_equal_value() {
        assert!(!check_deadband(1.0, 1.0, 0.0));
        assert!(check_deadband(1.0_f64.next_up(), 1.0, 0.0));
        assert!(check_deadband(1.0, 1.0_f64.next_up(), 0.0));
        assert!(check_deadband(f64::from_bits(1), 0.0, 0.0));
        assert!(check_deadband(0.0, -f64::from_bits(1), 0.0));
    }

    /// No "nothing posted yet" state: the first cycle compares VAL against
    /// the seed like every other cycle, whether the type deadbands against
    /// MLST/ALST (ai) or change-tests against OVAL at 0 (aSub).
    #[test]
    fn first_cycle_val_equal_to_the_seed_does_not_post() {
        use super::RecordInstance;
        use crate::server::records::ai::AiRecord;
        use crate::server::records::asub_record::ASubRecord;

        let mut asub = RecordInstance::new("A", ASubRecord::default());
        assert_eq!(asub.value_include_classes(), (false, false, false));
        let mut moved = ASubRecord::default();
        moved.val = 2;
        let mut asub = RecordInstance::new("A", moved);
        assert_eq!(asub.value_include_classes(), (true, true, false));

        let mut seeded = AiRecord::default();
        (seeded.val, seeded.mlst, seeded.alst) = (5.0, 5.0, 5.0);
        let mut ai = RecordInstance::new("B", seeded);
        assert_eq!(ai.check_deadband_ext(), (false, false));
    }

    /// C's compare is strict: a change of exactly the deadband does not post,
    /// in either direction, and one ulp below the deadband does.
    #[test]
    fn a_change_of_exactly_the_deadband_does_not_post() {
        assert!(!check_deadband(2.5, 1.0, 1.5));
        assert!(!check_deadband(1.0, 2.5, 1.5));
        assert!(check_deadband(2.5, 1.0, 1.5_f64.next_down()));
        assert!(check_deadband(1.0, 2.5, 1.5_f64.next_down()));
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
        let mut inst = RecordInstance::new("REC", AiRecord::default());
        let put = |inst: &mut RecordInstance, f: &str, v: &str| {
            inst.put_common_field_db_load(f, EpicsValue::String(v.into()))
                .unwrap_or_else(|e| panic!("put_common_field_db_load({f}, {v:?}) failed: {e}"));
        };

        // Integer-valued directives.
        put(&mut inst, "PHAS", "1");
        assert_eq!(inst.common.phas, 1, "field(PHAS, \"1\")");
        put(&mut inst, "TSE", "-2");
        assert_eq!(inst.common.tse(), -2, "field(TSE, \"-2\")");
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
        assert_eq!(
            inst.common.text.desc.as_str_lossy().as_ref(),
            "a description"
        );
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
        let mut inst = RecordInstance::new("DF", DfanoutRecord::default());

        // Untouched: reads its declared default (initial / type-zero), NOT an
        // error, and the override store is empty.
        assert_eq!(inst.resolve_field("HOPR"), Some(EpicsValue::Double(0.0)));
        assert!(inst.cold.declared_overrides.is_empty());

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
        let mut inst = RecordInstance::new("DF", DfanoutRecord::default());
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
        let mut inst = RecordInstance::new("DF", DfanoutRecord::default());
        assert!(matches!(
            inst.put_common_field("XYZZY", EpicsValue::String("1".into())),
            Err(CaError::FieldNotFound(_))
        ));
        assert!(inst.cold.declared_overrides.is_empty());
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
        let mut inst = RecordInstance::new("CO", CalcoutRecord::default());
        // PVAL is served by the record (its own storage), so it is not stored
        // in the override map; the map stays empty and no ghost cell shadows
        // the record's read.
        let _ = inst.put_common_field("PVAL", EpicsValue::String("1".into()));
        assert!(
            inst.cold.declared_overrides.is_empty(),
            "a field the record serves via get_field must not enter the override map"
        );
    }
}

#[cfg(test)]
mod pact_exit_tests {
    use super::*;
    use crate::server::records::ai::AiRecord;

    fn instance() -> RecordInstance {
        RecordInstance::new("PACT:REC", AiRecord::new(0.0))
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
    /// A hold inside a hold puts back the outer's 1, as C's nested
    /// `processTarget` restores the `pact` it saved (`dbDbLink.c:443`, `:514`).
    #[test]
    fn a_nested_chain_pact_hold_restores_the_outer_hold() {
        let cell = RecordCell::new(instance(), 0, false, None);
        let pact = || cell.read().common_field(CommonField::Pact);
        assert_eq!(pact(), Some(EpicsValue::Char(0)));
        let outer = cell.hold_chain_pact();
        let inner = cell.hold_chain_pact();
        drop(inner);
        assert_eq!(pact(), Some(EpicsValue::Char(1)), "still the outer hold");
        drop(outer);
        assert_eq!(pact(), Some(EpicsValue::Char(0)));
    }
}

#[cfg(test)]
mod subroutine_gate_tests {
    use super::*;
    use crate::server::records::ai::AiRecord;
    use crate::server::records::asub_record::ASubRecord;
    use crate::server::records::sub_record::SubRecord;

    /// The boundary of `ProcessPlan::runs_subroutine`: the cycle calls the
    /// subroutine runner exactly for the types that have a `do_sub`, and the
    /// skip flag is armed exactly for them, so a type the cycle never asks
    /// cannot carry a flag nothing clears.
    #[test]
    fn the_subroutine_runner_and_its_skip_flag_belong_to_sub_and_asub_alone() {
        let mut ai = RecordInstance::new("G:AI", AiRecord::new(0.0));
        let mut sub = RecordInstance::new("G:SUB", SubRecord::default());
        let mut asub = RecordInstance::new("G:ASUB", ASubRecord::default());
        for (inst, has) in [(&mut ai, false), (&mut sub, true), (&mut asub, true)] {
            assert_eq!(ProcessPlan::of(&*inst.record).runs_subroutine, has);
            inst.skip_subroutine_this_cycle();
            assert_eq!(inst.suppress_subroutine_run, has);
        }
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

    /// `ProcessPlan::simm` replaced `resolve_field("SIMM").is_some()` as the
    /// gate on `PvDatabase::check_simulation_mode`, so it must answer the same
    /// thing for every record type the port carries — a type that gained the
    /// simulation block while the plan said otherwise would silently stop
    /// simulating, and one that lost it would resolve SIML/SIOL by name on
    /// every process cycle again.
    #[test]
    fn the_simulation_gate_agrees_with_resolving_simm_on_every_record_type() {
        use crate::server::record::dbd_generated::RECORD_TYPE_ORDER;
        let mut declared = 0usize;
        for rtype in RECORD_TYPE_ORDER {
            let Ok(record) = crate::server::db_loader::create_record(rtype) else {
                continue;
            };
            let plan = ProcessPlan::of(record.as_ref());
            let instance = inst(&format!("SIM:{rtype}"), record);
            assert_eq!(
                plan.simm.is_some(),
                instance.resolve_field("SIMM").is_some(),
                "{rtype}"
            );
            declared += usize::from(plan.simm.is_some());
        }
        // Both arms have to be populated or the equality above is vacuous.
        assert!(declared > 0, "no record type declared a simulation block");
        assert!(
            declared < RECORD_TYPE_ORDER.len(),
            "every record type declared one"
        );
    }

    /// The two arms by name, so the gate's meaning is readable without
    /// running the sweep above: `ai` is `readValue`-bearing, `calc` has no
    /// `readValue` in C at all.
    /// The slot index is what lets the cycle take a pre-read link's text
    /// without searching for it by name, so it has to name the same link the
    /// search would have found — on every record type, not just the calc
    /// class it was measured on.
    #[test]
    fn the_metadata_link_slots_name_the_same_links_a_search_would_find() {
        use crate::server::record::dbd_generated::RECORD_TYPE_ORDER;
        let mut with_slots = 0usize;
        for rtype in RECORD_TYPE_ORDER {
            let Ok(record) = crate::server::db_loader::create_record(rtype) else {
                continue;
            };
            let instance = inst(&format!("S:{rtype}"), record);
            let links = instance.link_backed_metadata_links();
            let slots = instance.link_backed_metadata_input_slots();
            assert_eq!(links.len(), slots.len(), "{rtype}");
            for (lf, slot) in links.iter().zip(slots) {
                let multi = instance.record.multi_input_links();
                assert_eq!(
                    *slot,
                    multi.iter().position(|(mf, _)| mf == lf),
                    "{rtype}.{lf}"
                );
                if let Some(i) = *slot {
                    assert_eq!(multi[i].0, lf.as_str(), "{rtype}.{lf}");
                    with_slots += 1;
                }
            }
        }
        assert!(
            with_slots > 0,
            "no record type maps a metadata link onto its multi-input list"
        );
    }

    /// `link_text` replaced a materialise-then-test shape at six link-read
    /// sites, so what it owes them is that boundary: a link that reads empty
    /// is `None`, and every other answer is the text itself.
    #[test]
    fn link_text_answers_none_exactly_where_a_link_reads_empty() {
        use crate::server::record::dbd_generated::RECORD_TYPE_ORDER;
        for rtype in RECORD_TYPE_ORDER {
            let Ok(record) = crate::server::db_loader::create_record(rtype) else {
                continue;
            };
            let instance = inst(&format!("L:{rtype}"), record);
            let declared: Vec<&'static str> = instance
                .record
                .multi_input_links()
                .iter()
                .chain(instance.record.string_input_links())
                .map(|(lf, _)| *lf)
                .collect();
            for lf in declared {
                let materialised = match instance.record.get_field(lf) {
                    Some(EpicsValue::String(text)) => text,
                    _ => PvString::new(),
                };
                assert_eq!(
                    instance.link_text(lf),
                    (!materialised.is_empty()).then_some(materialised),
                    "{rtype}.{lf}"
                );
            }
        }
    }

    /// A record that lends a link text must lend the SAME text its
    /// `get_field` materialises — the two paths are one answer, and a slot
    /// mapping that drifts by one would otherwise hand the cycle a
    /// neighbouring link's target.
    #[test]
    fn a_lent_link_text_is_the_one_get_field_materialises() {
        use crate::server::record::dbd_generated::RECORD_TYPE_ORDER;
        for rtype in RECORD_TYPE_ORDER {
            let Ok(mut record) = crate::server::db_loader::create_record(rtype) else {
                continue;
            };
            let declared: Vec<&'static str> = record
                .multi_input_links()
                .iter()
                .chain(record.string_input_links())
                .map(|(lf, _)| *lf)
                .collect();
            // Distinct per slot, so a mapping off by one cannot agree.
            for (slot, lf) in declared.iter().enumerate() {
                let text = format!("SRC:{rtype}:{slot}.VAL CP");
                let _ = record.put_field(lf, EpicsValue::String(text.as_str().into()));
            }
            for lf in &declared {
                let Some(lent) = record.link_text_ref(lf) else {
                    continue;
                };
                let materialised = match record.get_field(lf) {
                    Some(EpicsValue::String(text)) => text,
                    _ => PvString::new(),
                };
                assert_eq!(
                    lent,
                    materialised.as_bytes(),
                    "{rtype}.{lf} lent a text its get_field does not hold"
                );
            }
        }
    }

    #[test]
    fn link_text_reads_a_wired_link_and_refuses_what_is_not_one() {
        let mut instance = inst("C:GOOD", Box::new(CalcRecord::default()));
        assert_eq!(instance.link_text("INPA"), None, "an unwired INPA");
        instance
            .record
            .put_field("INPA", EpicsValue::String("SRC:ONE.VAL CP".into()))
            .expect("INPA takes a link string");
        assert_eq!(
            instance.link_text("INPA").as_ref().map(PvString::as_bytes),
            Some(&b"SRC:ONE.VAL CP"[..])
        );
        // A numeric field is not a link, and neither is one the type does not
        // declare: both have to read as "no link", not as an empty one.
        assert_eq!(instance.link_text("A"), None);
        assert_eq!(instance.link_text("NOSUCH"), None);
    }

    #[test]
    fn a_record_type_without_readvalue_declares_no_simulation_block() {
        assert!(ProcessPlan::of(&CalcRecord::default()).simm.is_none());
        assert!(ProcessPlan::of(&BiRecord::default()).simm.is_some());
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
            assert_eq!(
                render_link_field(class, text.as_bytes()),
                want,
                "{class:?} {text:?}"
            );
        }
    }
}

#[cfg(test)]
mod unanswerable_notify_tests {
    use super::*;
    use crate::server::records::calc::CalcRecord;

    fn rec(name: &str) -> RecordInstance {
        RecordInstance::new(name, CalcRecord::default())
    }

    /// The ordinary case: the client is still waiting, so the slot is its own.
    /// C only reaches `dbNotifyCancel` from a teardown.
    #[test]
    fn a_waiting_client_keeps_the_slot() {
        let mut r = rec("A");
        let (tx, _rx) = crate::runtime::sync::oneshot::channel();
        r.install_or_queue_notify(tx).expect("slot was free");
        assert!(r.unanswerable_notify().is_none());
        assert!(r.has_notify(), "a live put-callback must not be cancelled");
    }

    /// C `rsrvFreePutNotify` (`camessage.c:1630-1638`): the client went away
    /// with its put-callback still busy, so the notify leaves the record.
    #[test]
    fn a_departed_client_releases_the_slot() {
        let mut r = rec("A");
        let (tx, rx) = crate::runtime::sync::oneshot::channel();
        r.install_or_queue_notify(tx).expect("slot was free");
        drop(rx);
        let dead = r.unanswerable_notify().expect("nobody can answer it");
        assert!(r.release_notify(&dead));
        assert!(
            !r.has_notify(),
            "the record must be free for the next put-notify"
        );
    }

    /// A notify that COMPLETED is not unanswerable — it answered. The sender is
    /// spent, so a receiver dropped afterwards says nothing about the slot.
    #[test]
    fn a_completed_notify_is_not_cancelled() {
        let mut r = rec("A");
        let (tx, rx) = crate::runtime::sync::oneshot::channel();
        let set = r.install_or_queue_notify(tx).expect("slot was free");
        set.leave();
        drop(rx);
        assert!(r.unanswerable_notify().is_none());
    }

    /// The set names every record that holds it, entry and `dbNotifyAdd`
    /// target alike — C `pnotifyPvt->waitList` (`dbNotify.c:227`/`:498`), which
    /// `dbNotifyCancel` walks at `:428`.
    #[test]
    fn the_set_names_the_entry_and_every_chain_target() {
        let mut entry = rec("A");
        let mut target = rec("B");
        let (tx, _rx) = crate::runtime::sync::oneshot::channel();
        let set = entry.install_or_queue_notify(tx).expect("slot was free");
        target.join_put_notify(Some(&set));
        let members: Vec<String> = set
            .joined_records()
            .into_iter()
            .map(|n| n.to_string())
            .collect();
        assert_eq!(members, vec!["A".to_string(), "B".to_string()]);
    }

    /// A chain target releases too. It holds the same wait-set through
    /// `dbNotifyAdd`, and the record whose cycle never ends is exactly the one
    /// that keeps it — a `busy` FLNK target left at VAL=1 declines its own
    /// `recGblFwdLink` by contract, so nothing else would ever free it. C
    /// empties the whole wait list (`dbNotify.c:428-430`) before it looks at
    /// the entry at all.
    #[test]
    fn a_chain_target_holding_the_same_set_releases_too() {
        let mut entry = rec("A");
        let mut target = rec("B");
        let (tx, rx) = crate::runtime::sync::oneshot::channel();
        let set = entry.install_or_queue_notify(tx).expect("slot was free");
        target.join_put_notify(Some(&set));
        drop(rx);
        let dead = target.unanswerable_notify().expect("nobody can answer it");
        assert!(Arc::ptr_eq(&dead, &set));
        assert!(target.release_notify(&dead));
        assert!(!target.has_notify(), "B must be free for the next put");
        assert!(entry.release_notify(&dead));
        assert!(!entry.has_notify());
    }

    /// The record's own slot is the authority, not the name the sweep carries:
    /// a member that has since completed and taken a LIVE notify keeps it.
    /// C re-tests `precord->ppn` for the same reason (`restartCheck`'s
    /// `assert(precord->ppn)`, `dbNotify.c:154`).
    #[test]
    fn a_member_that_moved_on_to_a_live_notify_is_left_alone() {
        let mut entry = rec("A");
        let mut target = rec("B");
        let (tx, rx) = crate::runtime::sync::oneshot::channel();
        let dead = entry.install_or_queue_notify(tx).expect("slot was free");
        target.join_put_notify(Some(&dead));
        drop(rx);

        // B finished its contribution and a fresh client took it.
        assert!(target.release_notify(&dead));
        let (tx2, _rx2) = crate::runtime::sync::oneshot::channel();
        target.install_or_queue_notify(tx2).expect("slot was free");

        assert!(
            !target.release_notify(&dead),
            "the stale name must not evict the live notify"
        );
        assert!(target.has_notify());
    }
}

#[cfg(test)]
mod claimed_field_tests {
    use super::*;
    use crate::server::recgbl::EventMask;
    use crate::server::record::{CycleMarks, CyclePostMask};
    use crate::types::DbFieldType;

    static FIELDS: &[crate::server::record::FieldDesc] = &[
        crate::server::record::FieldDesc::new("VAL", DbFieldType::Double, false),
        crate::server::record::FieldDesc::new("SENT", DbFieldType::Double, false),
    ];
    static SENT_POST: [(crate::server::record::FieldIx, CyclePostMask); 1] =
        crate::server::record::declared_post_table(
            FIELDS,
            [("SENT", CyclePostMask::MonitorValueLog)],
        );

    /// Throttle's shape: `process()` marks SENT and leaves VAL unmoved.
    #[derive(Default)]
    struct MarksSent {
        marked: bool,
    }

    impl Record for MarksSent {
        fn record_type(&self) -> &'static str {
            "marks_sent"
        }
        fn process(&mut self) -> CaResult<crate::server::record::ProcessOutcome> {
            self.marked = true;
            Ok(crate::server::record::ProcessOutcome::complete())
        }
        fn get_field(&self, name: &str) -> Option<EpicsValue> {
            match name {
                "VAL" | "SENT" => Some(EpicsValue::Double(1.0)),
                _ => None,
            }
        }
        fn put_field(&mut self, name: &str, _value: EpicsValue) -> CaResult<()> {
            Err(CaError::FieldNotFound(name.to_string()))
        }
        fn declared_fields(&self) -> &'static [crate::server::record::FieldDesc] {
            FIELDS
        }
        fn monitor_value_changed(&mut self) -> Option<bool> {
            Some(false)
        }
        fn cycle_hooks(&self) -> crate::server::record::CycleHooks {
            crate::server::record::CycleHooks::ALL
        }
        fn fields_posted_only_when_marked(&self) -> &'static [&'static str] {
            &["SENT"]
        }
        fn take_cycle_posted_fields(&mut self) -> CycleMarks {
            CycleMarks::of(&SENT_POST, u128::from(std::mem::take(&mut self.marked)))
        }
    }

    /// A claimed field keeps no published copy (I2): attach, the marked
    /// cycles and a direct publish all leave `last_posted` empty, and the
    /// subscriber still gets one post per mark, as from C's `monitor()`.
    #[test]
    fn a_claimed_field_is_never_published() {
        let mut inst = RecordInstance::new("R", MarksSent::default());
        inst.process_local().unwrap();
        assert!(inst.last_posted.is_empty(), "an unwatched marked cycle");
        let all = (EventMask::VALUE | EventMask::LOG).bits();
        let _sent = inst.add_subscriber("SENT", 2, DbFieldType::Double, all);
        for _ in 0..2 {
            let (snap, _) = inst.process_local().unwrap();
            assert_eq!(snap.iter().filter(|(f, _, _)| f == "SENT").count(), 1);
        }
        inst.record_value_post("SENT", PostedValue::Val(EpicsValue::Double(2.0)));
        assert!(inst.last_posted.is_empty(), "{:?}", inst.last_posted);
    }
}

#[cfg(test)]
mod forced_secondary_post_tests {
    use super::*;
    use crate::server::recgbl::EventMask;
    use crate::server::records::ao::AoRecord;
    use crate::types::DbFieldType;

    /// An ao whose output moved this cycle (`omod`) with `oraw != rval`,
    /// the state C's `aoRecord.c:541` posts RVAL from.
    fn ao_with_moved_rval(rval: i32) -> RecordInstance {
        let mut rec = AoRecord::default();
        rec.rval = rval;
        rec.omod = true;
        RecordInstance::new("AO:FORCED", rec)
    }

    /// One guarded monitor cycle; the fields it posted, in order.
    fn guarded_cycle(inst: &mut RecordInstance) -> Vec<String> {
        let mut snapshot = ProcessSnapshot::new();
        inst.collect_subscriber_posts(
            &mut snapshot,
            EventMask::VALUE,
            EventMask::NONE,
            true,
            false,
        );
        snapshot
            .iter()
            .map(|(field, _, _)| field.to_string())
            .collect()
    }

    /// Boundary `oraw != rval` with a subscriber: RVAL posts once and the
    /// old copy advances; boundary `oraw == rval` on the next guarded
    /// cycle: nothing.
    #[test]
    fn a_changed_rval_posts_once_then_not_again() {
        let mut inst = ao_with_moved_rval(7);
        let _rx = inst
            .add_subscriber("RVAL", 1, DbFieldType::Long, EventMask::VALUE.bits())
            .expect("RVAL subscriber");
        let posted = guarded_cycle(&mut inst);
        assert_eq!(
            posted.iter().filter(|f| *f == "RVAL").count(),
            1,
            "{posted:?}"
        );
        assert_eq!(inst.record.get_field("ORAW"), Some(EpicsValue::Long(7)));
        let posted = guarded_cycle(&mut inst);
        assert!(!posted.iter().any(|f| f == "RVAL"), "{posted:?}");
    }

    /// Every type's forced secondaries are read through their slots: posting
    /// each one to a subscriber reads no value by name.
    #[test]
    fn forced_secondaries_read_no_value_by_name() {
        use crate::server::records::{
            ai::AiRecord, bi::BiRecord, bo::BoRecord, busy::BusyRecord, calcout::CalcoutRecord,
            mbbi::MbbiRecord, mbbi_direct::MbbiDirectRecord, mbbo::MbboRecord,
            mbbo_direct::MbboDirectRecord,
        };
        let records: [Box<dyn Record>; 10] = [
            Box::new(AiRecord::default()),
            Box::new(AoRecord::default()),
            Box::new(BiRecord::default()),
            Box::new(BoRecord::default()),
            Box::new(BusyRecord::default()),
            Box::new(CalcoutRecord::default()),
            Box::new(MbbiRecord::default()),
            Box::new(MbbiDirectRecord::default()),
            Box::new(MbboRecord::default()),
            Box::new(MbboDirectRecord::default()),
        ];
        for rec in records {
            let rtype = rec.record_type();
            let mut inst = RecordInstance::new_boxed(format!("{rtype}:SLOT"), rec);
            let value_masked = inst.record.fields_posted_with_value_mask();
            let _rx: Vec<_> = value_masked
                .iter()
                .map(|(field, _)| {
                    let ty = inst.field_desc(field).unwrap().dbf_type;
                    inst.add_subscriber(field, 1, ty, EventMask::VALUE.bits())
                        .unwrap()
                })
                .collect();
            let mut snapshot = ProcessSnapshot::new();
            NAME_READS.with(|n| n.set(0));
            inst.post_forced_secondaries(
                &mut snapshot,
                value_masked,
                (1 << value_masked.len()) - 1,
                EventMask::VALUE,
                EventMask::VALUE,
            );
            assert_eq!(NAME_READS.with(|n| n.get()), 0, "{rtype}");
            assert_eq!(snapshot.len(), value_masked.len(), "{rtype}");
        }
    }

    /// The secondary block is taken once per cycle, whatever it fires: a type
    /// whose block can open on a shut VAL mask (bo, ao) every cycle, idle or
    /// not; one whose block sits inside VAL's guard (ai) only on a cycle VAL
    /// posts. Each boundary's old copy lands where C's block leaves it.
    #[test]
    fn the_secondary_block_is_taken_once_per_cycle() {
        use crate::server::records::{ai::AiRecord, bo::BoRecord};
        let cycle = |inst: &mut RecordInstance, mask: EventMask| {
            SECONDARY_TAKES.with(|n| n.set(0));
            inst.collect_subscriber_posts(
                &mut ProcessSnapshot::new(),
                mask,
                EventMask::NONE,
                true,
                false,
            );
            SECONDARY_TAKES.with(|n| n.get())
        };
        let mut bo = RecordInstance::new("BO:TAKE", BoRecord::default());
        assert_eq!(cycle(&mut bo, EventMask::NONE), 1, "bo idle");
        assert_eq!(cycle(&mut bo, EventMask::VALUE), 1, "bo VAL posts");
        let mut moved = BoRecord::default();
        moved.rval = 1;
        moved.rbv = 1;
        let mut bo = RecordInstance::new("BO:MOVED", moved);
        assert_eq!(cycle(&mut bo, EventMask::NONE), 1, "bo RVAL and RBV moved");
        assert_eq!(bo.record.get_field("ORAW"), Some(EpicsValue::ULong(1)));
        assert_eq!(bo.record.get_field("ORBV"), Some(EpicsValue::ULong(1)));

        let mut ai = RecordInstance::new("AI:TAKE", AiRecord::default());
        ai.record.put_field("RVAL", EpicsValue::Long(5)).unwrap();
        assert_eq!(cycle(&mut ai, EventMask::NONE), 0, "ai VAL shut");
        assert_eq!(ai.record.get_field("ORAW"), Some(EpicsValue::Long(0)));
        assert_eq!(cycle(&mut ai, EventMask::VALUE), 1, "ai VAL posts");
        assert_eq!(ai.record.get_field("ORAW"), Some(EpicsValue::Long(5)));

        let mut ao = RecordInstance::new("AO:TAKE", AoRecord::default());
        assert_eq!(cycle(&mut ao, EventMask::NONE), 1, "ao idle");
        let mut ao = ao_with_moved_rval(7);
        assert_eq!(cycle(&mut ao, EventMask::NONE), 1, "ao omod");
        assert_eq!(ao.record.get_field("ORAW"), Some(EpicsValue::Long(7)));
    }

    /// Boundary no subscriber: the old copy still advances, and nothing is
    /// posted — the bookkeeping must not depend on who is watching.
    #[test]
    fn a_changed_rval_advances_oraw_with_no_subscriber() {
        let mut inst = ao_with_moved_rval(7);
        assert!(guarded_cycle(&mut inst).is_empty());
        assert_eq!(inst.record.get_field("ORAW"), Some(EpicsValue::Long(7)));
    }

    /// Boundary unwatched vs watched: with no subscriber the fired OVAL and
    /// RVAL, which keep no published copy, are not looked up while ORAW still
    /// advances; one subscriber brings the lookups and its post back.
    #[test]
    fn an_unwatched_ao_skips_the_forced_secondary_lookups() {
        let passes = |inst: &mut RecordInstance| {
            FORCED_SECONDARY_VISITS.with(|n| n.set(0));
            let posted = guarded_cycle(inst);
            (FORCED_SECONDARY_VISITS.with(|n| n.get()), posted)
        };
        let mut inst = ao_with_moved_rval(7);
        assert_eq!(passes(&mut inst), (0, Vec::<String>::new()));
        assert_eq!(inst.record.get_field("ORAW"), Some(EpicsValue::Long(7)));

        let mut inst = ao_with_moved_rval(7);
        let _rx = inst
            .add_subscriber("RVAL", 1, DbFieldType::Long, EventMask::VALUE.bits())
            .expect("subscriber");
        let (visits, posted) = passes(&mut inst);
        assert!(visits > 0);
        assert_eq!(posted, vec![String::from("RVAL")]);
    }

    /// Boundary watched record, unwatched silent field: with RVAL alone
    /// watched, the fired OVAL, which keeps no published copy, is neither
    /// read nor handed to the published log.
    #[test]
    fn a_watched_ao_skips_its_unwatched_silent_secondary() {
        let mut inst = ao_with_moved_rval(7);
        let _rx = inst
            .add_subscriber("RVAL", 1, DbFieldType::Long, EventMask::VALUE.bits())
            .expect("subscriber");
        STORELESS_POSTS.with(|p| p.borrow_mut().clear());
        assert_eq!(guarded_cycle(&mut inst), ["RVAL"]);
        let storeless = STORELESS_POSTS.with(|p| p.take());
        assert!(!storeless.iter().any(|f| f == "OVAL"), "{storeless:?}");
        assert_eq!(inst.record.get_field("ORAW"), Some(EpicsValue::Long(7)));
    }

    /// I2 for ao: watched OVAL, RVAL and RBV post from the record's own
    /// copies and leave no published copy behind, RBV only on `orbv != rbv`.
    #[test]
    fn ao_keeps_no_published_copy_of_its_posts() {
        let mut rbv_moved = AoRecord::default();
        rbv_moved.rbv = 3;
        let instances = [
            (ao_with_moved_rval(7), ["OVAL", "RVAL"]),
            (RecordInstance::new("AO:RBV", rbv_moved), ["OVAL", "RBV"]),
        ];
        for (mut inst, first) in instances {
            for (sid, field) in ["OVAL", "RVAL", "RBV"].into_iter().enumerate() {
                let _rx = inst
                    .add_subscriber(
                        field,
                        sid as u32,
                        DbFieldType::Double,
                        EventMask::VALUE.bits(),
                    )
                    .expect("subscriber");
            }
            assert_eq!(guarded_cycle(&mut inst), first);
            assert_eq!(guarded_cycle(&mut inst), ["OVAL"]);
            assert!(inst.last_posted.is_empty(), "{:?}", inst.last_posted);
        }
    }

    /// ai RVAL rides VAL's own guard (aiRecord.c:459-465): a shut guard
    /// leaves ORAW stale, an open one posts RVAL with VAL's mask and advances
    /// ORAW, subscriber or not.
    #[test]
    fn ai_oraw_advances_only_inside_vals_guard() {
        use crate::server::records::ai::AiRecord;
        let mut rec = AiRecord::default();
        rec.rval = 7;
        let mut inst = RecordInstance::new("AI:RVAL", rec);
        let cycle = |inst: &mut RecordInstance, mask| {
            let mut snapshot = ProcessSnapshot::new();
            inst.collect_subscriber_posts(&mut snapshot, mask, EventMask::NONE, true, false);
            snapshot
                .iter()
                .map(|(field, _, mask)| (field.to_string(), *mask))
                .collect::<Vec<_>>()
        };

        assert!(cycle(&mut inst, EventMask::NONE).is_empty());
        assert_eq!(inst.record.get_field("ORAW"), Some(EpicsValue::Long(0)));

        assert!(cycle(&mut inst, EventMask::LOG).is_empty());
        assert_eq!(inst.record.get_field("ORAW"), Some(EpicsValue::Long(7)));

        inst.record.put_field("RVAL", EpicsValue::Long(9)).unwrap();
        let _rx = inst
            .add_subscriber("RVAL", 1, DbFieldType::Long, EventMask::VALUE.bits())
            .expect("RVAL subscriber");
        let posted = cycle(&mut inst, EventMask::VALUE);
        assert_eq!(posted, [("RVAL".to_string(), EventMask::VALUE)]);
        assert_eq!(inst.record.get_field("ORAW"), Some(EpicsValue::Long(9)));
    }
}

#[cfg(test)]
mod mlis_tests {
    //! The record's subscriber list against C's `mlis` walk
    //! (`dbEvent.c:876-906`), one test per boundary.
    use super::*;
    use crate::server::event_queue::{EventReader, TryRecvError};
    use crate::server::recgbl::EventMask;
    use crate::server::records::ao::AoRecord;
    use crate::types::DbFieldType;

    fn ao() -> RecordInstance {
        RecordInstance::new("AO:MLIS", AoRecord::default())
    }

    fn attach(inst: &mut RecordInstance, field: &str, mask: EventMask) -> EventReader {
        inst.add_subscriber(field, 0, DbFieldType::Double, mask.bits())
            .expect("subscriber")
    }

    /// A cycle's post of `field` under its declared name.
    fn post(inst: &RecordInstance, field: &str, mask: EventMask) {
        let desc = field_desc_of(inst.record.as_ref(), field).unwrap();
        let mut snapshot = ProcessSnapshot::new();
        snapshot.push((
            PostName::Declared(desc),
            PostedValue::Val(EpicsValue::Double(1.0)),
            mask,
        ));
        inst.notify_from_snapshot(&snapshot);
    }

    fn drained(reader: &mut EventReader) -> Vec<u16> {
        std::iter::from_fn(|| reader.try_recv().ok())
            .map(|ev| ev.mask().bits())
            .collect()
    }

    /// 0, 1, 2 and 3 monitors on one field: each receives the post once, and
    /// the third spills the list past its two inline entries.
    #[test]
    fn every_monitor_up_to_the_spill_receives_one_post() {
        for n in 0..=3 {
            let mut inst = ao();
            let mut readers: Vec<_> = (0..n)
                .map(|_| attach(&mut inst, "VAL", EventMask::VALUE))
                .collect();
            assert_eq!(inst.subscribers.spilled(), n > 2, "{n} monitors");
            post(&inst, "VAL", EventMask::VALUE);
            for reader in &mut readers {
                assert_eq!(drained(reader), [EventMask::VALUE.bits()], "{n} monitors");
            }
        }
    }

    /// An ACKT put that lowers ACKS and an ACKS put post ACKT, ACKS and the
    /// record-wide alarm by declaration: no name byte is read.
    #[test]
    fn an_acknowledge_put_reads_no_name() {
        let mut inst = ao();
        let mut readers: Vec<_> = ["ACKT", "ACKS", "VAL"]
            .into_iter()
            .map(|field| attach(&mut inst, field, EventMask::VALUE | EventMask::ALARM))
            .collect();
        inst.common.acks = AlarmSeverity::Major;
        inst.common.ackt = true;
        NAME_BYTE_COMPARES.with(|n| n.set(0));
        inst.put_ackt(0);
        inst.common.acks = AlarmSeverity::Minor;
        inst.put_acks(AlarmSeverity::Minor as u16);
        assert_eq!(NAME_BYTE_COMPARES.with(|n| n.get()), 0);
        let got: Vec<_> = readers.iter_mut().map(|r| drained(r).len()).collect();
        // C putAckt (dbAccess.c:1295-1301) posts ACKT, ACKS and the
        // record-wide alarm; putAcks (:1313-1314) ACKS and the alarm again.
        assert_eq!(got, [3, 4, 2]);
    }

    /// Monitors on two fields of one record each receive only their own.
    #[test]
    fn a_post_reaches_only_its_fields_monitors() {
        let mut inst = ao();
        let mut val = attach(&mut inst, "VAL", EventMask::VALUE);
        let mut oval = attach(&mut inst, "OVAL", EventMask::VALUE);
        post(&inst, "OVAL", EventMask::VALUE);
        assert!(drained(&mut val).is_empty());
        assert_eq!(drained(&mut oval), [EventMask::VALUE.bits()]);
        post(&inst, "VAL", EventMask::VALUE);
        assert_eq!(drained(&mut val), [EventMask::VALUE.bits()]);
        assert!(drained(&mut oval).is_empty());
    }

    /// One field watched for VALUE and for ALARM: `caEventMask & select`
    /// decides each (`dbEvent.c:896-900`).
    #[test]
    fn one_fields_monitors_are_selected_by_mask() {
        let mut inst = ao();
        let mut value = attach(&mut inst, "VAL", EventMask::VALUE);
        let mut alarm = attach(&mut inst, "VAL", EventMask::ALARM);
        post(&inst, "VAL", EventMask::VALUE);
        post(&inst, "VAL", EventMask::ALARM);
        post(&inst, "VAL", EventMask::VALUE | EventMask::ALARM);
        assert_eq!(
            drained(&mut value),
            [EventMask::VALUE.bits(), EventMask::VALUE.bits()]
        );
        assert_eq!(
            drained(&mut alarm),
            [EventMask::ALARM.bits(), EventMask::ALARM.bits()]
        );
    }

    /// Detached between two posts of one burst: the detached monitor keeps
    /// what was queued and is then closed; the other receives both.
    #[test]
    fn a_monitor_detached_mid_burst_closes_alone() {
        let mut inst = ao();
        let mut gone = attach(&mut inst, "VAL", EventMask::VALUE);
        let mut kept = attach(&mut inst, "VAL", EventMask::VALUE);
        let mut burst = PostBurst::new();
        let val = inst.field_desc("VAL").unwrap();
        inst.notify_declared(Some(&mut burst), val, EventMask::VALUE);
        inst.remove_subscriber(gone.id());
        inst.notify_declared(Some(&mut burst), val, EventMask::VALUE);
        drop(burst);
        assert!(gone.try_recv().is_ok());
        assert_eq!(gone.try_recv().err(), Some(TryRecvError::Disconnected));
        assert_eq!(drained(&mut kept).len(), 2);
    }

    /// A list detached empty after spilling reads as unwatched, and the next
    /// monitor attached receives its posts.
    #[test]
    fn a_list_emptied_after_spilling_posts_to_its_next_monitor() {
        let mut inst = ao();
        let gone: Vec<_> = (0..3)
            .map(|_| attach(&mut inst, "VAL", EventMask::VALUE))
            .collect();
        for reader in &gone {
            inst.remove_subscriber(reader.id());
        }
        assert!(inst.subscribers.spilled() && inst.unwatched());
        post(&inst, "VAL", EventMask::VALUE);
        inst.notify_field("VAL", EventMask::VALUE);
        let mut next = attach(&mut inst, "VAL", EventMask::VALUE);
        post(&inst, "VAL", EventMask::VALUE);
        assert_eq!(drained(&mut next), [EventMask::VALUE.bits()]);
    }

    /// [`MonitorHot::WATCHED`] follows the list through each change: the first
    /// attach, a detach that leaves one, the last detach of a spilled list, a
    /// re-attach and destroy. `unwatched` asserts it against the list.
    #[test]
    fn the_watched_bit_follows_every_change_to_the_list() {
        let mut inst = ao();
        assert!(inst.unwatched());
        let first = attach(&mut inst, "VAL", EventMask::VALUE);
        assert!(!inst.unwatched());
        let rest: Vec<_> = (0..2)
            .map(|_| attach(&mut inst, "VAL", EventMask::VALUE))
            .collect();
        inst.remove_subscriber(first.id());
        assert!(!inst.unwatched());
        for reader in &rest {
            inst.remove_subscriber(reader.id());
        }
        assert!(inst.subscribers.spilled() && inst.unwatched());
        let _again = attach(&mut inst, "VAL", EventMask::VALUE);
        assert!(!inst.unwatched());
        inst.destroy();
        assert!(inst.unwatched());
    }

    /// With no monitor the post stops at the list's length.
    #[test]
    fn an_unwatched_record_never_reaches_delivery() {
        let inst = ao();
        DELIVERIES.with(|n| n.set(0));
        post(&inst, "VAL", EventMask::VALUE);
        assert_eq!(DELIVERIES.with(|n| n.get()), 0);
    }

    /// A post no monitor's mask passes builds no log head; one passing post
    /// builds it once for all its monitors.
    #[test]
    fn a_log_head_is_built_only_for_a_passing_post() {
        let mut inst = ao();
        let _readers: Vec<_> = (0..2)
            .map(|_| attach(&mut inst, "VAL", EventMask::VALUE))
            .collect();
        HEADS.with(|n| n.set(0));
        post(&inst, "VAL", EventMask::ALARM);
        inst.notify_field("VAL", EventMask::ALARM);
        assert_eq!(HEADS.with(|n| n.get()), 0);
        post(&inst, "VAL", EventMask::VALUE);
        assert_eq!(HEADS.with(|n| n.get()), 1);
        inst.notify_field("VAL", EventMask::VALUE);
        assert_eq!(HEADS.with(|n| n.get()), 2);
    }

    /// A watched record's cycle that posts nothing stops at the snapshot,
    /// and the next cycle's post still lands.
    #[test]
    fn a_cycle_with_no_post_never_reaches_delivery() {
        let mut inst = ao();
        let mut reader = attach(&mut inst, "VAL", EventMask::VALUE);
        DELIVERIES.with(|n| n.set(0));
        inst.notify_from_snapshot(&ProcessSnapshot::new());
        assert_eq!(DELIVERIES.with(|n| n.get()), 0);
        post(&inst, "VAL", EventMask::VALUE);
        assert_eq!(DELIVERIES.with(|n| n.get()), 1);
        assert_eq!(drained(&mut reader), [EventMask::VALUE.bits()]);
    }

    /// A declared field's fixed-list answers — value-mask gate, aux class,
    /// previous-copy bit — come by index and equal what its name finds.
    #[test]
    fn a_declared_fields_fixed_roles_read_no_name_bytes() {
        use crate::server::records::{
            calcout::CalcoutRecord, event::EventRecord, transform::TransformRecord,
        };
        let records: [Box<dyn Record>; 4] = [
            Box::new(AoRecord::default()),
            Box::new(CalcoutRecord::default()),
            Box::new(EventRecord::default()),
            Box::new(TransformRecord::default()),
        ];
        for record in records {
            let plan = MonitorPlan::of(record.as_ref());
            let declared = super::super::dbd_generated::DB_COMMON_FIELDS
                .iter()
                .chain(record.field_list());
            let mut found = 0;
            for (i, desc) in declared.enumerate() {
                NAME_BYTE_COMPARES.with(|n| n.set(0));
                let by_ix = plan.fixed_roles(PostKey::Ix(FieldIx(i as u16)));
                assert_eq!(NAME_BYTE_COMPARES.with(|n| n.get()), 0, "{}", desc.name);
                assert_eq!(
                    by_ix,
                    plan.fixed_roles(PostKey::Named(desc.name)),
                    "{}",
                    desc.name
                );
                found += usize::from(by_ix != FixedRoles::default());
            }
            assert!(found > 0, "{}", record.record_type());
        }
    }

    #[test]
    fn records_of_one_type_share_their_field_bits() {
        use crate::server::records::calcout::CalcoutRecord;
        let (a, b) = (AoRecord::default(), AoRecord::default());
        let (a, b) = (MonitorPlan::of(&a), MonitorPlan::of(&b));
        assert!(std::ptr::eq(a.claimed_bits.0, b.claimed_bits.0));
        assert!(std::ptr::eq(a.walk_skip_bits.0, b.walk_skip_bits.0));
        let calcout = MonitorPlan::of(&CalcoutRecord::default());
        assert_ne!(a.walk_skip_bits.0, calcout.walk_skip_bits.0);
    }

    /// A pending pass's notify posts the record's own value of each field it
    /// names, found by index, and only a changed one.
    #[test]
    fn an_async_notify_posts_by_index_and_reads_no_name_bytes() {
        use crate::server::record::{dbd_generated::CALCOUT_FIELDS, declared_field_ix};
        use crate::server::records::calcout::CalcoutRecord;
        let mut inst = RecordInstance::new("CO:NFY", CalcoutRecord::default());
        let mut reader = attach(&mut inst, "A", EventMask::VALUE);
        const A: FieldIx = declared_field_ix(CALCOUT_FIELDS, "A");
        inst.record.put_field("A", EpicsValue::Double(3.0)).unwrap();
        NAME_BYTE_COMPARES.with(|n| n.set(0));
        let posts = inst.collect_notify_posts(vec![A]);
        assert_eq!(NAME_BYTE_COMPARES.with(|n| n.get()), 0);
        assert_eq!(posts.len(), 1);
        inst.notify_from_snapshot(&posts);
        let values: Vec<_> = std::iter::from_fn(|| reader.try_recv().ok())
            .map(|ev| ev.value().as_value().cloned())
            .collect();
        assert_eq!(values, [Some(EpicsValue::Double(3.0))]);
        assert_eq!(inst.collect_notify_posts(vec![A]).len(), 0, "unchanged");
    }

    /// The per-instance sets — forced, alarm fan-out, the marks and the closed
    /// process set — settle every published entry and monitor by index.
    #[test]
    fn a_declared_set_settle_reads_no_name_bytes() {
        use crate::server::records::{asub_record::ASubRecord, calcout::CalcoutRecord};
        let mut calcout = RecordInstance::new("CO:SET", CalcoutRecord::default());
        let mut asub = RecordInstance::new("AS:SET", ASubRecord::default());
        asub.record
            .put_field("EFLG", EpicsValue::Short(2))
            .expect("EFLG ALWAYS");
        for (inst, fields) in [
            (&mut calcout, &["VAL", "A", "L", "DLYA", "DESC"][..]),
            (&mut asub, &["VAL", "VALA", "NEVU", "A"][..]),
        ] {
            let _readers: Vec<_> = fields
                .iter()
                .map(|field| attach(inst, field, EventMask::VALUE))
                .collect();
            for field in fields {
                inst.record_value_post(field, PostedValue::Val(EpicsValue::Double(0.0)));
            }
            let lists = Lists {
                forced: inst.record.force_posted_fields(),
                fanout: inst.record.alarm_cycle_monitored_fields(),
                swept: inst.record.log_swept_fields(),
                marks: inst.record.take_process_posted_fields().table(),
            };
            let closed = inst.record.process_posted_fields();
            NAME_BYTE_COMPARES.with(|n| n.set(0));
            inst.resettle_roles(lists);
            inst.resettle_posted(closed);
            assert_eq!(NAME_BYTE_COMPARES.with(|n| n.get()), 0, "{}", inst.name);
            // Each monitor carries the roles and membership its entry does.
            let entry = |inst: &RecordInstance, field: &str| {
                let sub = inst
                    .subscribers
                    .iter()
                    .find(|sub| sub.field() == field)
                    .expect("attached");
                let roles = &sub.roles;
                (roles.forced(), roles.fanout(), roles.marks != 0, sub.posted)
            };
            let want: &[(&str, (bool, bool, bool, bool))] = if fields.contains(&"DLYA") {
                &[
                    ("VAL", (false, false, false, true)),
                    ("A", (false, true, false, true)),
                    ("L", (false, true, false, true)),
                    ("DLYA", (false, false, true, false)),
                    ("DESC", (false, false, false, false)),
                ]
            } else {
                &[
                    ("VAL", (false, false, false, true)),
                    ("VALA", (true, false, false, true)),
                    ("NEVU", (true, false, false, true)),
                    ("A", (false, false, false, false)),
                ]
            };
            for &(field, roles) in want {
                assert_eq!(entry(inst, field), roles, "{} {field}", inst.name);
            }
        }
    }

    /// A cycle's declared-field posts — the deadband VAL, the forced OVAL and
    /// RVAL — find their monitors, and pass the others, by pointer alone.
    #[test]
    fn a_declared_field_post_compares_no_name_bytes() {
        let mut rec = AoRecord::default();
        rec.rval = 7;
        rec.omod = true;
        let mut inst = RecordInstance::new("AO:PTR", rec);
        let mut readers: Vec<_> = ["VAL", "OVAL", "RVAL"]
            .into_iter()
            .map(|field| attach(&mut inst, field, EventMask::VALUE))
            .collect();
        NAME_BYTE_COMPARES.with(|n| n.set(0));
        let mut snapshot = ProcessSnapshot::new();
        inst.deadband_post(&mut snapshot, EventMask::NONE, true, false);
        inst.collect_subscriber_posts(
            &mut snapshot,
            EventMask::VALUE,
            EventMask::NONE,
            true,
            false,
        );
        inst.notify_from_snapshot(&snapshot);
        assert_eq!(NAME_BYTE_COMPARES.with(|n| n.get()), 0);
        for reader in &mut readers {
            assert_eq!(drained(reader).len(), 1);
        }
    }

    /// A cycle that posts nothing does the same work whoever is subscribed: an
    /// idle bo or stringout never tests its subscriber set or enters the walk,
    /// with no monitor, one on the deadband VAL, one on a claimed field (RVAL)
    /// or one on a field the cycle never posts (DESC). A cycle that moves RVAL,
    /// a posted field, walks and delivers.
    #[epics_macros_rs::epics_test]
    async fn an_idle_cycle_does_the_same_work_whoever_is_subscribed() {
        let (db, _) = crate::server::ioc_builder::IocBuilder::new()
            .db_string(
                r#"record(bo, "BO:IDLE") {}
                record(stringout, "SO:IDLE") {}"#,
                &Default::default(),
            )
            .unwrap()
            .build()
            .await
            .unwrap();
        let cycles = || {
            let db = db.clone();
            async move {
                for _ in 0..8 {
                    for name in ["BO:IDLE", "SO:IDLE"] {
                        let mut visited = crate::server::database::ProcStack::new();
                        db.process_record_with_links(name, &mut visited)
                            .await
                            .unwrap();
                    }
                }
            }
        };
        let counted = || {
            (
                SUBSCRIBER_TESTS.with(|n| n.replace(0)),
                WALKS.with(|n| n.replace(0)),
                DELIVERIES.with(|n| n.replace(0)),
            )
        };
        // The first cycle clears UDF's alarm and posts.
        cycles().await;
        counted();
        cycles().await;
        assert_eq!(counted(), (0, 0, 0), "unwatched");
        let mut readers = Vec::new();
        for (name, field) in [
            ("BO:IDLE", "VAL"),
            ("BO:IDLE", "RVAL"),
            ("BO:IDLE", "DESC"),
            ("SO:IDLE", "VAL"),
            ("SO:IDLE", "DESC"),
        ] {
            let rec = db.get_record(name).unwrap();
            readers.push(
                rec.write()
                    .add_subscriber(field, 1, DbFieldType::Double, EventMask::VALUE.bits())
                    .unwrap(),
            );
        }
        counted();
        cycles().await;
        assert_eq!(counted(), (0, 0, 0), "watched");
        db.put_record_field_from_ca_no_notify("BO:IDLE", "VAL", EpicsValue::Double(1.0))
            .await
            .unwrap();
        let (_, walks, deliveries) = counted();
        assert!(
            walks > 0 && deliveries > 0,
            "RVAL moved: {walks} walks, {deliveries} deliveries"
        );
        assert!(!drained(&mut readers[1]).is_empty());
    }

    /// Every declared field of every type reads through the plan's route as
    /// the name chain answers it, and an unreadable one is refused.
    #[test]
    fn every_declared_read_answers_as_the_name_chain() {
        use crate::server::record::dbd_generated::{DB_COMMON_FIELDS, RECORD_TYPES};
        let mut checked = 0usize;
        for record_type in RECORD_TYPES {
            let inst = RecordInstance::new_boxed(
                format!("T:{record_type}"),
                crate::server::record::record_trait::tests::any_record(record_type),
            );
            for desc in DB_COMMON_FIELDS.iter().chain(inst.record.field_list()) {
                let read = inst.resolve_field_stored_at(desc.name, Some(desc));
                if desc.unreadable() {
                    assert!(read.is_none(), "{record_type}.{}", desc.name);
                    continue;
                }
                let Some(chain) = inst
                    .record
                    .get_field(desc.name)
                    .or_else(|| inst.get_common_field(desc.name))
                else {
                    continue;
                };
                assert_eq!(
                    format!("{read:?}"),
                    format!("{:?}", Some(chain)),
                    "{record_type}.{}",
                    desc.name
                );
                checked += 1;
            }
        }
        assert!(checked > 1000, "{checked} reads");
    }

    /// `FieldRead::table` settles a common-named own field once, from the
    /// construction-time `get_field` answer; that holds only while no type
    /// answers such a name in one state and leaves it to the instance in
    /// another. Every declared field is put to flip what state it can.
    #[test]
    fn a_common_named_own_field_is_owned_in_every_state() {
        use crate::server::record::dbd_generated::RECORD_TYPES;
        let owned = |record: &dyn Record| -> Vec<&'static str> {
            record
                .field_list()
                .iter()
                .filter(|d| CommonField::by_name(d.name).is_some())
                .filter(|d| record.get_field(d.name).is_some())
                .map(|d| d.name)
                .collect()
        };
        let mut owning = 0usize;
        for record_type in RECORD_TYPES {
            let mut record = crate::server::record::record_trait::tests::any_record(record_type);
            let fresh = owned(&*record);
            owning += fresh.len();
            for desc in record.field_list() {
                for value in [
                    EpicsValue::Double(1.0),
                    EpicsValue::Short(1),
                    EpicsValue::String("X.VAL".into()),
                ] {
                    let _ = record.put_field(desc.name, value);
                }
            }
            assert_eq!(owned(&*record), fresh, "{record_type}");
        }
        assert!(owning >= 28, "{owning} owned common names");
    }

    /// A record whose one monitor watches the field its cycle posts: a
    /// by-name match there answers from `ptr::eq` before any byte, so only a
    /// count taken on entry to `watches_str` sees it.
    #[epics_macros_rs::epics_test]
    async fn a_lone_monitor_post_takes_no_name() {
        let (db, _) = crate::server::ioc_builder::IocBuilder::new()
            .db_string(
                r#"record(calcout, "CL:PTR") { field(CALC, "A") field(ODLY, "0.1") }"#,
                &Default::default(),
            )
            .unwrap()
            .build()
            .await
            .unwrap();
        let rec = db.get_record("CL:PTR").unwrap();
        let mut rx = rec
            .write()
            .add_subscriber("DLYA", 1, DbFieldType::UShort, EventMask::VALUE.bits())
            .unwrap();
        NAME_BYTE_COMPARES.with(|n| n.set(0));
        db.put_record_field_from_ca_no_notify("CL:PTR", "PROC", EpicsValue::Short(1))
            .await
            .unwrap();
        crate::runtime::task::sleep(std::time::Duration::from_millis(300)).await;
        assert_eq!(NAME_BYTE_COMPARES.with(|n| n.get()), 0);
        assert_eq!(drained(&mut rx).len(), 2, "DLYA set and cleared");
    }

    /// Monitored bo, ao, stringout and calcout cycles and put-time posts walk,
    /// match, publish and reap by index and pointer from the first publish:
    /// the deadband VAL, a `dbCommon` field the walk always skips, the
    /// secondaries RVAL/OVAL, a calcout input, the alarm posts, a non-pp field
    /// and a `prop(YES)` put's record-wide post read no name byte, and no
    /// lookup takes a name.
    #[epics_macros_rs::epics_test]
    async fn a_monitored_cycle_and_put_read_no_name_bytes() {
        let (db, _) = crate::server::ioc_builder::IocBuilder::new()
            .db_string(
                r#"record(bo, "BO:PTR") {}
                record(ao, "AO:PTR") { field(MDEL, "-1") }
                record(stringout, "SO:PTR") {}
                record(calcout, "CO:PTR") { field(CALC, "A+B") }
                record(calcout, "CD:PTR") { field(CALC, "A") field(ODLY, "0.2") }
                record(calc, "CP:PTR") {}
                record(calc, "CF:PTR") { field(FLNK, "CP:PTR") }"#,
                &Default::default(),
            )
            .unwrap()
            .build()
            .await
            .unwrap();
        let watch = [
            ("BO:PTR", &["VAL", "RVAL", "SEVR", "HIGH"][..]),
            ("AO:PTR", &["VAL", "RVAL", "OVAL", "SEVR", "HOPR"][..]),
            ("SO:PTR", &["VAL", "SEVR", "DESC"][..]),
            ("CO:PTR", &["VAL", "A"][..]),
            ("CD:PTR", &["DLYA"][..]),
            ("CP:PTR", &["VAL", "SEVR"][..]),
        ];
        let mut readers = Vec::new();
        for (name, fields) in watch {
            let rec = db.get_record(name).unwrap();
            for field in fields {
                readers.push(
                    rec.write()
                        .add_subscriber(field, 1, DbFieldType::Double, EventMask::VALUE.bits())
                        .unwrap(),
                );
            }
        }
        let put = |rec: &'static str, field: &'static str, v: EpicsValue| {
            let db = db.clone();
            async move {
                db.put_record_field_from_ca_no_notify(rec, field, v)
                    .await
                    .unwrap();
            }
        };
        let d = EpicsValue::Double;
        let s = |v: &str| EpicsValue::String(v.into());
        // From the first publish: settling a field's roles reads no name.
        NAME_BYTE_COMPARES.with(|n| n.set(0));
        put("BO:PTR", "VAL", d(1.0)).await;
        put("BO:PTR", "HIGH", d(1.0)).await;
        put("AO:PTR", "VAL", d(1.0)).await;
        put("AO:PTR", "HOPR", d(1.0)).await;
        put("SO:PTR", "VAL", s("a")).await;
        put("SO:PTR", "DESC", s("a")).await;
        for v in [0.0, 1.0, 0.0] {
            put("BO:PTR", "VAL", d(v)).await;
            put("AO:PTR", "VAL", d(v + 2.0)).await;
        }
        for v in ["b", "c"] {
            put("SO:PTR", "VAL", s(v)).await;
        }
        put("BO:PTR", "HIGH", d(2.0)).await;
        put("AO:PTR", "HOPR", d(2.0)).await;
        put("SO:PTR", "DESC", s("b")).await;
        for v in [1.0, 2.0] {
            put("CO:PTR", "A", d(v)).await;
            put("CO:PTR", "B", d(v)).await;
        }
        // The ODLY start marks DLYA.
        put("CD:PTR", "A", d(1.0)).await;
        // A target held active past a completed cycle refuses the forward
        // link's entries until the MAX_LOCK raise posts the alarm fields and VAL.
        {
            let rec = db.get_record("CP:PTR").unwrap();
            let mut inst = rec.write();
            inst.common.udf = 0;
            inst.common.sevr = AlarmSeverity::NoAlarm;
            inst.common.stat = crate::server::recgbl::alarm_status::NO_ALARM;
            inst.enter_pact();
        }
        for _ in 0..11 {
            put("CF:PTR", "PROC", d(1.0)).await;
        }
        assert_eq!(NAME_BYTE_COMPARES.with(|n| n.get()), 0);
        for reader in &mut readers {
            assert!(!drained(reader).is_empty());
        }
    }
}

#[cfg(test)]
mod skipped_posted_tests {
    use super::*;
    use crate::server::recgbl::EventMask;
    use crate::server::records::calc::CalcRecord;
    use crate::types::DbFieldType;

    fn calc() -> RecordInstance {
        RecordInstance::new("CALC:SKIP", CalcRecord::default())
    }

    /// Boundary: a field the walk always skips. Its published value must not
    /// land in `last_posted`, or the walk's gate never fires again — the first
    /// alarm transition posts SEVR/STAT/AMSG/ACKS, so that one entry cost
    /// every later cycle of every record the whole walk.
    #[test]
    fn a_skipped_field_post_leaves_the_gate_armed() {
        for field in ["SEVR", "STAT", "AMSG", "ACKS", "UDF"] {
            let mut inst = calc();
            inst.record_value_post(field, PostedValue::Val(EpicsValue::Long(1)));
            assert!(
                inst.last_posted.is_empty(),
                "{field} reached the walk's input"
            );
            assert_eq!(
                inst.posted_value(field),
                Some(&PostedValue::Val(EpicsValue::Long(1)))
            );
        }
    }

    /// Boundary: the deadband field. Its published value is MLST/ALST, so a
    /// post of it — a `dbPut` to VAL with nobody watching — stores nothing.
    #[test]
    fn a_deadband_field_post_stores_nothing() {
        let mut inst = calc();
        inst.record_value_post("VAL", PostedValue::Val(EpicsValue::Double(1.0)));
        inst.notify_field("VAL", EventMask::VALUE | EventMask::LOG);
        assert!(inst.last_posted.is_empty() && inst.cold.skipped_posted.is_empty());
        assert_eq!(inst.posted_value("VAL"), None);
    }

    /// A monitor on a tracker field seeds its published value through the
    /// owner; that seed must not land in `last_posted`, or the walk runs every
    /// cycle after the monitor goes away. (A `caput` is refused: SPC_NOMOD.)
    #[epics_macros_rs::epics_test]
    async fn a_tracker_field_monitor_leaves_the_gate_armed() {
        use crate::server::database::ProcStack;
        use crate::server::ioc_builder::IocBuilder;
        let (db, _) = IocBuilder::new()
            .db_string(
                r#"record(calc, "C") { field(CALC, "A+1") }
                   record(ao, "O") { field(VAL, "1") }"#,
                &std::collections::HashMap::new(),
            )
            .expect("db")
            .build()
            .await
            .expect("ioc");
        for (rec, field) in [
            ("C", "MLST"),
            ("C", "ALST"),
            ("C", "LALM"),
            ("O", "MLST"),
            ("O", "ALST"),
            ("O", "LALM"),
            ("O", "ORAW"),
            ("O", "ORBV"),
        ] {
            {
                let inst = db.get_record(rec).unwrap();
                let mut inst = inst.write();
                let rx = inst
                    .add_subscriber(field, 1, DbFieldType::Double, EventMask::VALUE.bits())
                    .expect("subscriber");
                inst.remove_subscriber(rx.id());
            }
            db.process_record_with_links(rec, &mut ProcStack::new())
                .await
                .unwrap();
            let inst = db.get_record(rec).unwrap();
            let inst = inst.read();
            // ao's own cycle posts RVAL, so only calc's gate stays armed.
            assert!(
                !inst.last_posted.contains(inst.post_key(field))
                    && (rec != "C" || inst.last_posted.is_empty()),
                "{rec}.{field} reached the walk's input: {:?}",
                inst.last_posted
            );
            assert!(inst.posted_value(field).is_some(), "{rec}.{field}");
        }
    }

    /// A marked claimed array (acalcout's AA, stored by its CALC every
    /// cycle) with no monitor stops at `post_put_field`'s `reap_closed` and
    /// leaves no `last_posted` entry, so no later walk reads it; with one
    /// monitor each cycle posts it once.
    #[epics_macros_rs::epics_test]
    async fn a_marked_claimed_array_is_read_only_for_its_monitor() {
        use crate::server::database::ProcStack;
        use crate::server::ioc_builder::IocBuilder;
        use crate::server::records::acalcout::AcalcoutRecord;
        let (db, _) = IocBuilder::new()
            .register_record_type("acalcout", || Box::new(AcalcoutRecord::new()))
            .db_string(
                r#"record(acalcout, "AC") { field(NELM, "4") field(CALC, "AA:=IX;0") }"#,
                &std::collections::HashMap::new(),
            )
            .expect("db")
            .build()
            .await
            .expect("ioc");
        db.ioc_init().await;
        let cell = db.get_record("AC").unwrap();
        for _ in 0..2 {
            db.process_record_with_links("AC", &mut ProcStack::new())
                .await
                .unwrap();
            let inst = cell.read();
            assert!(
                !inst.last_posted.contains(inst.post_key("AA")),
                "AA reached the walk with no monitor: {:?}",
                inst.last_posted
            );
        }
        let mut rx = cell
            .write()
            .add_subscriber("AA", 1, DbFieldType::Double, EventMask::VALUE.bits())
            .expect("subscriber");
        for _ in 0..2 {
            db.process_record_with_links("AC", &mut ProcStack::new())
                .await
                .unwrap();
            assert!(rx.try_recv().is_ok(), "the marked AA posted to its monitor");
            assert!(rx.try_recv().is_err(), "AA posted more than once a cycle");
        }
    }

    /// The opposite boundary: a field the walk CAN act on must disarm the
    /// gate, or the walk would stop change-detecting it.
    #[test]
    fn a_walkable_field_post_disarms_the_gate() {
        let mut inst = calc();
        inst.record_value_post("PREC", PostedValue::Val(EpicsValue::Long(3)));
        assert!(!inst.last_posted.is_empty());
        assert_eq!(
            inst.posted_value("PREC"),
            Some(&PostedValue::Val(EpicsValue::Long(3)))
        );
    }

    /// The skipped store is still a map: a second post advances the value in
    /// place rather than shadowing it with a second entry.
    #[test]
    fn a_second_skipped_post_advances_in_place() {
        let mut inst = calc();
        inst.record_value_post("SEVR", PostedValue::Val(EpicsValue::Long(1)));
        inst.record_value_post("SEVR", PostedValue::Val(EpicsValue::Long(2)));
        assert_eq!(inst.cold.skipped_posted.len(), 1);
        assert_eq!(
            inst.posted_value("SEVR"),
            Some(&PostedValue::Val(EpicsValue::Long(2)))
        );
    }

    /// `add_subscriber` seeds the published value through the owner, so
    /// subscribing to a skipped field does not re-arm the walk either.
    #[test]
    fn subscribing_to_a_skipped_field_seeds_through_the_owner() {
        let mut inst = calc();
        let _rx = inst
            .add_subscriber("SEVR", 1, DbFieldType::Short, EventMask::VALUE.bits())
            .expect("SEVR subscriber");
        assert!(inst.last_posted.is_empty());
        assert!(inst.posted_value("SEVR").is_some());
    }
}

#[cfg(test)]
mod walk_post_name_tests {
    use super::*;
    use crate::server::recgbl::EventMask;
    use crate::types::DbFieldType;
    use epics_macros_rs::EpicsRecord;

    /// Borrows ai's field declarations and claims no `monitor()` post, so
    /// the walk change-detects HOPR. These tests go with the walk (M1).
    #[derive(Default, EpicsRecord)]
    #[record(type = "ai", crate_path = "crate")]
    struct WalkProbe {
        #[field(type = "Double")]
        val: f64,
        #[field(type = "Double")]
        hopr: f64,
    }

    /// The walk's posts for one cycle with `key` subscribed and HOPR, a
    /// change-detected field, moved since.
    fn hopr_posts(key: &str) -> Vec<PostName> {
        let mut inst = RecordInstance::new("AI:NAME", WalkProbe::default());
        let _rx = inst
            .add_subscriber(key, 1, DbFieldType::Double, EventMask::VALUE.bits())
            .expect("subscriber");
        inst.record
            .put_field("HOPR", EpicsValue::Double(7.0))
            .unwrap();
        let mut snapshot = ProcessSnapshot::new();
        inst.collect_subscriber_posts(
            &mut snapshot,
            EventMask::VALUE,
            EventMask::NONE,
            true,
            false,
        );
        snapshot.iter().map(|(field, _, _)| field.clone()).collect()
    }

    /// A field keyed by its declared spelling posts that `&'static` name, so
    /// the post copies nothing under the record lock.
    #[test]
    fn a_declared_field_posts_its_static_name() {
        assert!(matches!(
            hopr_posts("HOPR")[..],
            [PostName::Declared(FieldDesc { name: "HOPR", .. })]
        ));
    }

    /// Any other spelling posts as keyed: delivery looks the subscriber up
    /// by that key.
    #[test]
    fn another_spelling_posts_as_keyed() {
        let posts = hopr_posts("hopr");
        assert!(
            matches!(&posts[..], [PostName::Named(Cow::Owned(name))] if name == "hopr"),
            "{posts:?}"
        );
    }
}

#[cfg(test)]
mod posted_settle_tests {
    use super::*;
    use crate::server::recgbl::EventMask;
    use crate::server::records::asub_record::ASubRecord;
    use crate::types::DbFieldType;

    fn walk(inst: &mut RecordInstance) {
        let mut snapshot = ProcessSnapshot::new();
        inst.collect_subscriber_posts(
            &mut snapshot,
            EventMask::NONE,
            EventMask::NONE,
            false,
            false,
        );
    }

    fn posted(inst: &mut RecordInstance, field: &str) -> bool {
        inst.settle_posted();
        let key = inst.post_key(field);
        inst.last_posted.get(key).unwrap().posted()
    }

    static TOGGLES_FIELDS: &[crate::server::record::FieldDesc] = &[
        crate::server::record::FieldDesc::new("VAL", DbFieldType::Double, false),
        crate::server::record::FieldDesc::new("X", DbFieldType::Double, false),
    ];

    /// A closed list that answers by state, as scaler's channel count or
    /// aSub's EFLG does, over a field the type does not claim.
    struct Toggles {
        closed: bool,
    }

    impl Record for Toggles {
        fn record_type(&self) -> &'static str {
            "toggles"
        }
        fn process(&mut self) -> CaResult<crate::server::record::ProcessOutcome> {
            Ok(crate::server::record::ProcessOutcome::complete())
        }
        fn get_field(&self, name: &str) -> Option<EpicsValue> {
            matches!(name, "VAL" | "X").then_some(EpicsValue::Double(0.0))
        }
        fn put_field(&mut self, name: &str, _value: EpicsValue) -> CaResult<()> {
            Err(CaError::FieldNotFound(name.to_string()))
        }
        fn declared_fields(&self) -> &'static [crate::server::record::FieldDesc] {
            TOGGLES_FIELDS
        }
        fn process_posted_fields(&self) -> Option<crate::server::record::DeclaredFields> {
            Some(if self.closed {
                &const { crate::server::record::declared_set(TOGGLES_FIELDS, ["VAL"]) }
            } else {
                &const { crate::server::record::declared_set(TOGGLES_FIELDS, ["VAL", "X"]) }
            })
        }
    }

    fn toggles(closed: bool) -> RecordInstance {
        let mut inst = RecordInstance::new("T", Toggles { closed });
        let _rx = inst
            .add_subscriber("X", 1, DbFieldType::Double, EventMask::VALUE.bits())
            .expect("X subscriber");
        inst
    }

    /// A state change moves the closed list both ways; each walk after it
    /// re-settles every entry against the list it now answers, the field it
    /// closes out included.
    #[test]
    fn a_moved_list_resettles_every_published_membership() {
        let mut inst = toggles(false);
        for closed in [true, false, true] {
            inst.record = Box::new(Toggles { closed });
            walk(&mut inst);
            assert_eq!(posted(&mut inst, "X"), !closed, "closed {closed}");
        }
    }

    /// The deadband post reads its field's membership from the record's list
    /// now: transform's closed list leaves VAL out, aSub's keeps it.
    #[test]
    fn the_deadband_post_reads_the_listed_membership() {
        use crate::server::records::transform::TransformRecord;
        let mut transform = RecordInstance::new("T", TransformRecord::default());
        let mut asub = RecordInstance::new("AS", ASubRecord::default());
        for (inst, member) in [(&mut transform, false), (&mut asub, true)] {
            let _rx = inst
                .add_subscriber("VAL", 1, DbFieldType::Double, EventMask::ALARM.bits())
                .expect("VAL subscriber");
            let mut snapshot = ProcessSnapshot::new();
            inst.deadband_post(&mut snapshot, EventMask::ALARM, true, true);
            assert_eq!(inst.deadband_posted(), member, "{}", inst.name);
            assert_eq!(snapshot.len(), usize::from(member), "{}", inst.name);
        }
    }

    /// A list that drops and readmits the deadband field, as aSub's EFLG does.
    struct DropsVal {
        closed: bool,
    }

    impl Record for DropsVal {
        fn record_type(&self) -> &'static str {
            "dropsval"
        }
        fn process(&mut self) -> CaResult<crate::server::record::ProcessOutcome> {
            Ok(crate::server::record::ProcessOutcome::complete())
        }
        fn get_field(&self, name: &str) -> Option<EpicsValue> {
            matches!(name, "VAL" | "X").then_some(EpicsValue::Double(0.0))
        }
        fn put_field(&mut self, name: &str, _value: EpicsValue) -> CaResult<()> {
            Err(CaError::FieldNotFound(name.to_string()))
        }
        fn declared_fields(&self) -> &'static [crate::server::record::FieldDesc] {
            TOGGLES_FIELDS
        }
        fn process_posted_fields(&self) -> Option<crate::server::record::DeclaredFields> {
            Some(if self.closed {
                &const { crate::server::record::declared_set(TOGGLES_FIELDS, ["X"]) }
            } else {
                &const { crate::server::record::declared_set(TOGGLES_FIELDS, ["VAL", "X"]) }
            })
        }
    }

    /// With no monitor and nothing published, the deadband post's CP mask
    /// follows the list the record answers each cycle, though no walk runs to
    /// settle `posted` against it.
    #[test]
    fn an_unwatched_deadband_post_follows_a_turned_over_list() {
        let mut inst = RecordInstance::new("D".to_string(), DropsVal { closed: false });
        for closed in [false, true, false, true] {
            inst.record = Box::new(DropsVal { closed });
            let mut snapshot = ProcessSnapshot::new();
            let mask = inst.deadband_post(&mut snapshot, EventMask::ALARM, true, false);
            assert!(inst.unwatched() && inst.walk_unreached());
            assert_eq!(mask, EventMask::ALARM | EventMask::VALUE);
            let published = if closed { EventMask::NONE } else { mask };
            assert_eq!(snapshot.published_mask(), published, "closed {closed}");
        }
    }

    /// The first publish of a field with no monitor sets
    /// [`MonitorHot::PUBLISHED`], and the walk then has a field to reach;
    /// `walk_unreached` asserts the bit against `last_posted`.
    #[test]
    fn a_first_unwatched_publish_opens_the_walk() {
        let mut inst = RecordInstance::new("T".to_string(), Toggles { closed: false });
        assert!(inst.walk_unreached());
        inst.record_value_post("X", PostedValue::Val(EpicsValue::Double(1.0)));
        assert!(inst.unwatched() && !inst.walk_unreached());
    }

    /// A field first published while the instance still holds the old list
    /// reads the current membership: the read settles first.
    #[test]
    fn a_field_published_before_the_first_settle_reads_settled() {
        assert!(posted(&mut toggles(false), "X"));
    }
}

#[cfg(test)]
mod link_cache_fact_tests {
    use super::*;
    use crate::server::record::{LinkFieldType, parse_link_field};
    use crate::server::records::ai::AiRecord;

    /// The entry's inline facts are the installed parse's answers, through
    /// every kind change and back to empty: a fact left from the previous
    /// parse would make the cycle skip or take a link its text no longer has.
    #[test]
    fn every_install_restates_the_entrys_facts() {
        let mut inst = RecordInstance::new("LCF", AiRecord::new(0.0));
        let fresh = &inst.parsed_out;
        assert!(fresh.is_constant() && !fresh.is_writable_out_link());
        for text in ["OTHER.VAL", "5", "ca://OTHER", "@hw 1", "OTHER NPP", ""] {
            let parse = parse_link_field(text, LinkFieldType::Out);
            for field in ["OUT", "SDIS", "FLNK"] {
                assert!(inst.set_link_cache(field, parse.clone()));
            }
            let facts = |constant: bool, writable_out: bool, link: &ParsedLink| {
                assert_eq!(
                    constant,
                    crate::server::recgbl::simm::is_constant(link),
                    "{text:?}"
                );
                assert_eq!(writable_out, link.is_writable_out_link(), "{text:?}");
            };
            let out = &inst.parsed_out;
            facts(out.is_constant(), out.is_writable_out_link(), out.link());
            for cache in [&inst.parsed_sdis, &inst.parsed_flnk] {
                facts(
                    cache.is_constant(),
                    cache.is_writable_out_link(),
                    cache.link(),
                );
            }
        }
    }

    /// A DB link's PROC target, else its PP, and its MS reach the facts;
    /// any other kind answers no gate and NMS.
    #[test]
    fn the_out_facts_follow_the_parse() {
        use crate::server::database::ProcessTargetGate::{ProcField, ScanPassive};
        use crate::server::record::MonitorSwitch;
        let mut inst = RecordInstance::new("LCF", AiRecord::new(0.0));
        for (text, gate, switch) in [
            ("OTHER.PROC PP MS", Some(ProcField), MonitorSwitch::Maximize),
            ("OTHER.PROC NPP", Some(ProcField), MonitorSwitch::NoMaximize),
            ("OTHER.VAL PP", Some(ScanPassive), MonitorSwitch::NoMaximize),
            ("OTHER.VAL NPP MSI", None, MonitorSwitch::MaximizeIfInvalid),
            // C `dbParseLink` defaults an output link to NPP.
            ("OTHER", None, MonitorSwitch::NoMaximize),
            ("ca://OTHER.PROC MSS", None, MonitorSwitch::MaximizeStatus),
            ("5", None, MonitorSwitch::NoMaximize),
        ] {
            assert!(inst.set_link_cache("OUT", parse_link_field(text, LinkFieldType::Out)));
            let cache = &inst.parsed_out;
            assert_eq!(
                (cache.process_gate(), cache.monitor_switch()),
                (gate, switch),
                "{text:?}"
            );
        }
    }

    /// Both sites that let the lent entry go empty the target first: a link
    /// swap, and `release_link_addrs` behind `release_link_targets_to` and
    /// `destroy`.
    #[test]
    fn the_out_target_empties_where_its_entry_can_go() {
        use crate::server::records::ao::AoRecord;
        let target = RecordCell::new(
            RecordInstance::new("TGT", AoRecord::new(0.0)),
            0,
            false,
            None,
        );
        let put = PutAddr::resolve_in(&target, "VAL");
        let mut inst = RecordInstance::new("SRC", AoRecord::new(0.0));
        let parse = parse_link_field("TGT.VAL PP", LinkFieldType::Out);
        let lend = |inst: &mut RecordInstance| {
            inst.parsed_out
                .set_out_target(Some(link_cache::OutTarget::new(&target, put, 7)));
            assert_eq!(inst.parsed_out.out_target().map(|t| t.revision()), Some(7));
        };

        assert!(inst.set_link_cache("OUT", parse.clone()));
        lend(&mut inst);
        assert!(inst.set_link_cache("OUT", parse.clone()));
        assert!(inst.parsed_out.out_target().is_none(), "swap");

        lend(&mut inst);
        inst.release_link_targets_to(&target);
        assert!(
            inst.parsed_out.out_target().is_none(),
            "release_link_targets_to"
        );

        lend(&mut inst);
        inst.destroy();
        assert!(inst.parsed_out.out_target().is_none(), "destroy");
    }
}

#[cfg(test)]
mod put_addr_tests {
    use super::*;
    use crate::server::database::{ProcStack, PvDatabase};
    use crate::server::ioc_builder::IocBuilder;
    use crate::server::record::dbd_generated::RECORD_TYPES;
    use crate::server::record::{FieldSlot, SlotRef};

    /// One `R` per record type the builder loads with no fields set.
    async fn every_type() -> Vec<std::sync::Arc<PvDatabase>> {
        let mut dbs = Vec::new();
        for record_type in RECORD_TYPES {
            let db = format!(r#"record({record_type}, "R") {{}}"#);
            let Ok(builder) = IocBuilder::new().db_string(&db, &Default::default()) else {
                continue;
            };
            if let Ok((db, _)) = builder.build().await {
                dbs.push(db);
            }
        }
        dbs
    }

    /// `(field, slot, cap)` of every field `dbPut` stores through a lean
    /// `Str` arm.
    fn lean_strings(rec: &RecordCell) -> Vec<(&'static str, FieldSlot, u16)> {
        rec.rdes()
            .fields
            .iter()
            .filter_map(|d| match PutAddr::resolve_in(rec, d.name).put() {
                PutFn::Str { slot, cap } => Some((d.name, slot, cap)),
                _ => None,
            })
            .collect()
    }

    fn assert_reserved(db: &PvDatabase, name: &str, when: &str) -> usize {
        let rec = db.get_record(name).unwrap();
        let fields = lean_strings(&rec);
        let inst = rec.read();
        for &(field, slot, cap) in &fields {
            let Some(SlotRef::Str(s)) = inst.record.slot_ref(slot) else {
                panic!("{name}.{field}: no Str slot");
            };
            assert!(
                s.capacity() >= usize::from(cap),
                "{name}.{field} {when}: capacity {} < {cap}",
                s.capacity()
            );
        }
        fields.len()
    }

    /// A lean `Str` store copies within the reserve `RecordCell::new` gave
    /// the slot; any writer that replaces the buffer instead makes the next
    /// lean store grow it under the hold.
    #[epics_macros_rs::epics_test]
    async fn every_lean_string_slot_keeps_its_reserve() {
        let mut checked = 0;
        for db in every_type().await {
            checked += assert_reserved(&db, "R", "at load");
            let rec = db.get_record("R").unwrap();
            for (field, _, _) in lean_strings(&rec) {
                let _ = db
                    .put_pv(&format!("R.{field}"), EpicsValue::String("x".into()))
                    .await;
            }
            assert_reserved(&db, "R", "after a put by name");
            let _ = db
                .process_record_with_links("R", &mut ProcStack::new())
                .await;
            assert_reserved(&db, "R", "after a process cycle");
        }
        assert!(checked > 0, "no lean string field found");

        // stringin's simulation SVAL and stringout's IVOA "Set output to
        // IVOV" are the writers a plain cycle does not reach.
        let (db, _) = IocBuilder::new()
            .db_string(
                r#"
record(stringout, "SRC") { field(VAL, "simulated") }
record(stringin, "SI") { field(SIMM, "YES") field(SIOL, "SRC NPP") }
record(ai, "BAD") { field(PINI, "YES") field(VAL, "5") field(HIHI, "1") field(HHSV, "INVALID") }
record(stringout, "SOI") { field(OMSL, "closed_loop") field(DOL, "BAD MS")
                           field(IVOA, "Set output to IVOV") field(IVOV, "iv") }
"#,
                &Default::default(),
            )
            .unwrap()
            .build()
            .await
            .unwrap();
        for name in ["BAD", "SI", "SOI"] {
            db.process_record_with_links(name, &mut ProcStack::new())
                .await
                .unwrap();
        }
        assert_eq!(
            db.get_pv("SI").unwrap(),
            EpicsValue::String("simulated".into())
        );
        assert_eq!(db.get_pv("SOI").unwrap(), EpicsValue::String("iv".into()));
        assert_reserved(&db, "SI", "after simulation");
        assert_reserved(&db, "SOI", "after IVOA");
    }

    /// `lean_put_fn` takes a lean arm only when the slot's recorded variant
    /// is the declared type's; any other variant is `General`.
    #[epics_macros_rs::epics_test]
    async fn a_disagreeing_slot_variant_takes_general() {
        use SlotKind as K;
        const KINDS: [SlotKind; 11] = [
            K::F64,
            K::F32,
            K::I8,
            K::U8,
            K::I16,
            K::U16,
            K::I32,
            K::U32,
            K::I64,
            K::U64,
            K::Str,
        ];
        let mut checked = 0;
        for db in every_type().await {
            let rec = db.get_record("R").unwrap();
            let rdes = rec.rdes();
            for (i, desc) in rdes.fields.iter().enumerate() {
                let (Some(slot), Some(recorded)) = (rec.slots[i], rec.put_facts[i].kind) else {
                    continue;
                };
                if lean_put_fn(rdes.record_type, desc, slot, recorded) == PutFn::General {
                    continue;
                }
                for kind in KINDS.into_iter().filter(|&k| k != recorded) {
                    assert_eq!(
                        lean_put_fn(rdes.record_type, desc, slot, kind),
                        PutFn::General,
                        "{}.{} as {kind:?}",
                        rdes.record_type,
                        desc.name
                    );
                }
                checked += 1;
            }
        }
        assert!(checked > 0);
    }
}
