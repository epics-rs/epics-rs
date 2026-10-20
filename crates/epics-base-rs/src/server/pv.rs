use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use crate::error::CaError;
use crate::server::database::filters::FilterChain;
use crate::server::event_queue::{
    AttachId, DeliveryContext, EventReader, EventSink, EventUser, PostBurst, PostOutcome, RowOwner,
    SubscriberList, TryRecvError, no_context,
};
use crate::server::snapshot::{ControlInfo, DisplayInfo, EnumInfo, PropertySupport, Snapshot};
use crate::types::{DbFieldType, EpicsValue, RecordName, WallTime};

/// Why a monitor was not installed. C `db_add_event` returns NULL for both and
/// each caller prints its own line, so the reason is handed back, not logged.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubscribeRefused {
    /// No such record or PV, or it was destroyed.
    NoRecord,
    /// The per-PV subscriber cap (`EPICS_CAS_MAX_SUBSCRIBERS_PER_PV`).
    CapReached,
    /// C `dbChannelCreate` found no such field (`pvNameLookup`,
    /// `dbChannel.c:460-462`), or a `$` on a field that is neither a string
    /// nor a link (`:500-503`).
    FieldNotFound,
    /// C `dbChannelCreate`'s `chf_parse` stopped on the filter suffix
    /// (`dbChannel.c:513-516`), having printed C's line.
    BadFilter,
}

/// A reader's drop unlinks its monitor, so the post walks live rows only.
impl RowOwner for ProcessVariable {
    fn remove_row(&self, id: AttachId) {
        drop(self.remove_subscriber(id));
    }
}

/// A destroyed PV's closed list refuses as a missing PV does.
impl From<crate::server::event_queue::Closed> for SubscribeRefused {
    fn from(_: crate::server::event_queue::Closed) -> Self {
        Self::NoRecord
    }
}

/// Per-PV subscriber cap. Default 1024 — comfortably above
/// any realistic dashboard fan-out, small enough to bound the
/// per-PV `Vec<Subscriber>` under abuse. Override via
/// `EPICS_CAS_MAX_SUBSCRIBERS_PER_PV`.
pub(crate) fn max_subscribers_per_pv() -> usize {
    crate::runtime::env::get_lossy("EPICS_CAS_MAX_SUBSCRIBERS_PER_PV")
        .and_then(|s| s.parse::<usize>().ok())
        .unwrap_or(1024)
        .max(8)
}

/// Process-global counter of monitor events the subscriber never observed
/// because a later post replaced them in the event queue — C `evSubscrip
/// ::nreplace` (`dbEvent.c:821`), summed over every monitor. Covers both
/// `ProcessVariable` and `RecordInstance` posts, because both reach the queue
/// through the single [`EventSink::post`] owner. Mirrors the pattern of
/// `dropped_monitors` on the client side (subscribe_with_deadband).
///
/// read via [`dropped_monitor_events`]. That reader is not yet
/// wired to a live scrape surface — the `/queues` admin endpoint
/// currently renders configured limits only, not this counter — so do
/// not assume the value is observable through an endpoint until that
/// wiring lands.
static DROPPED_MONITOR_EVENTS: AtomicU64 = AtomicU64::new(0);

/// Read the cumulative count of dropped monitor events. Intended for
/// introspection / metrics; see `DROPPED_MONITOR_EVENTS` for the
/// current wiring status.
pub fn dropped_monitor_events() -> u64 {
    DROPPED_MONITOR_EVENTS.load(Ordering::Relaxed)
}

/// Identity of the client driving a `WriteHook` invocation. Carries
/// the user/host/peer fields the CA TCP handler already tracks for
/// audit + access security, so a proxy hook (gateway, ACL filter,
/// putlog) can make decisions without re-deriving them.
#[derive(Debug, Clone, Default)]
pub struct WriteContext {
    /// CA `CLIENT_NAME` username as the client sent it, or empty if unknown.
    pub user: Vec<u8>,
    /// CA `HOST_NAME` bytes as the client sent them (or the peer IP), used
    /// for ACF matching against `HAG(...)` groups.
    pub host: Vec<u8>,
    /// Raw `peer.ip():peer.port()` string, retained for audit/log use.
    pub peer: String,
}

/// Async hook invoked by client-originated writes (CA `caput`, CA
/// `WRITE_NOTIFY`) before the PV's local value is set. Used by the CA
/// gateway and similar proxies to forward writes upstream instead of
/// landing them in the local `ProcessVariable`.
///
/// The hook receives the proposed new value plus a [`WriteContext`]
/// identifying the client, and must return either:
/// * `Ok(())` — the write was accepted (e.g. forwarded to upstream).
///   The caller does NOT update the local `value` field — the
///   subsequent upstream-monitor event is expected to do that. This
///   matches CA-gateway semantics where the cached value reflects
///   reality after the round-trip.
/// * `Err(CaError)` — the write was rejected. The caller surfaces
///   the error to the CA client (`WRITE_NOTIFY` carries the ECA
///   status). The hook itself decides whether to update local state
///   on rejection.
///
/// The hook is consulted only on the client → server path. Internal
/// callers (`ProcessVariable::set`, `put_pv_and_post`) bypass it so
/// the upstream-monitor forwarder can update local state without
/// recursing into itself.
///
/// ## Stale-local hazard
///
/// "Hook returns `Ok` → caller does NOT update local value" assumes
/// the upstream will emit a monitor event reflecting the new value.
/// EPICS records can violate that assumption: PP=NO fields,
/// PUT-only fields (e.g. `.PROC`), and records configured to suppress
/// monitor events on identical values. In those cases the shadow
/// PV remains at its pre-put value indefinitely — caput appears to
/// succeed but `caget` afterwards returns the old value.
///
/// Hook implementors who target such records SHOULD update the local
/// `ProcessVariable` themselves on `Ok` — typically by invoking
/// `pv.set(new_value).await` AFTER the upstream put-ack, accepting
/// the cost of one local mutation per put. The base hook contract
/// stays "do nothing on Ok" because most monitor-driven shadows
/// (the CA gateway's primary use case) WILL receive a monitor event
/// and updating locally would race with it.
///
/// ## Reentrancy
///
/// The TCP write path clones the hook `Arc` and releases the read
/// guard BEFORE invoking it, so a hook that calls
/// `pv.set_write_hook(...)` to swap itself does not deadlock. A hook
/// that calls `pv.set(...)` reentrantly is allowed but defeats the
/// "let the upstream-monitor update local state" contract — the
/// reentrant `set` will be silently overwritten by the next
/// upstream event.
pub type WriteHook = Arc<
    dyn Fn(
            EpicsValue,
            WriteContext,
        )
            -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), CaError>> + Send>>
        + Send
        + Sync,
>;

/// read/write access decision for a gateway shadow PV,
/// evaluated for a specific downstream `(user, host)`. Mirrors the CA
/// access-rights model the server reports to the client and gates
/// reads on.
#[derive(Debug, Clone, Copy)]
pub struct AccessDecision {
    /// Client may GET / MONITOR (`EVENT_ADD`) the PV.
    pub read: bool,
    /// Client may PUT (`WRITE` / `WRITE_NOTIFY`) the PV.
    pub write: bool,
}

/// per-PV access hook installed by a proxy (the CA
/// gateway) so the CA server routes a shadow PV's access-rights
/// decision through the proxy's own ACF instead of the server's.
/// Given the downstream client's `(user, host)`, it returns the
/// [`AccessDecision`].
///
/// Symmetric to [`WriteHook`]: the gateway captures its single
/// `ArcSwap<AccessConfig>` and the PV's `.pvlist` ASG/ASL in the
/// closure, so `compute_access` reports access rights and gates reads
/// with the same `can_read` / `can_write` the write hook uses — one
/// ACF authority, no second copy to keep in sync. The hook is
/// synchronous (it only reads an in-memory `ArcSwap`, no `.await`); the
/// server consults it at `CREATE_CHAN` and on access-rights
/// re-evaluation.
pub type AccessHook = Arc<dyn Fn(&[u8], &[u8]) -> AccessDecision + Send + Sync>;

/// per-PV read hook consulted by the CA server's one-shot GET path
/// (`CA_PROTO_READ` / `CA_PROTO_READ_NOTIFY`) when set. A bare PV serves
/// reads straight from its stored value cell; a proxy (the CA gateway in
/// its no-cache mode) installs this hook so each downstream GET is
/// satisfied by a *fresh* upstream fetch instead of the last cached
/// value. Mirrors C ca-gateway `-no_cache`, where a connected channel
/// with caching disabled forwards every read as a fresh
/// `ca_array_get_callback()` to the IOC (`gateVc.cc:1361-1369`) rather
/// than returning `vc->eventData()`.
///
/// The hook returns a full [`Snapshot`], not a bare value: C `-no_cache`
/// reads issue `ca_array_get_callback(eventType(), ...)` with `eventType()`
/// a `DBR_TIME_*` class, and `getTimeCB` decodes the event's status,
/// severity, and timestamp into `setEventData` before the GET completes
/// (`gatePv.cc:976`, `:1789-1794`). The hook therefore owns producing the
/// fresh value *together with* its upstream alarm/timestamp so the read
/// path never synthesizes metadata by grafting a fresh value onto an
/// unrelated cached snapshot. Property metadata (display/control/enum) is
/// not carried by a `DBR_TIME_*` event in either C or here; the consumer
/// overlays the shadow's last-known property metadata for those fields
/// (a separate upstream path feeds them, as C splits value/time from the
/// property monitor).
///
/// The hook is async (it performs an upstream get) and fallible: on
/// `Err` the server surfaces the failure to the client (`ECA_GETFAIL`)
/// exactly as the IOC's own get-callback error would propagate. Only the
/// GET path consults it ([`ProcessVariable::read_snapshot`]); monitor
/// fan-out, the initial monitor event, and access-rights re-posts keep
/// serving the stored snapshot, so a no-cache PV still backs a downstream
/// monitor with its upstream subscription's events.
///
/// `None` (the default) leaves the read path byte-for-byte unchanged for
/// every record-backed and cached PV — the hook is purely additive.
pub type ReadHook = Arc<
    dyn Fn()
            -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<Snapshot, CaError>> + Send>>
        + Send
        + Sync,
>;

/// C `union native_value` (`db_field_log.h:42-53`): one scalar no wider than
/// 8 bytes, with its type. `DB_EVENT_LOG_STRINGS` is undefined, so a string
/// is never one.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum NativeValue {
    Short(i16),
    Float(f32),
    Enum(u16),
    Char(u8),
    Long(i32),
    Double(f64),
    Int64(i64),
    UInt64(u64),
    UShort(u16),
    ULong(u32),
    UChar(u8),
}

impl NativeValue {
    fn of(value: &EpicsValue) -> Option<Self> {
        Some(match *value {
            EpicsValue::Short(v) => Self::Short(v),
            EpicsValue::Float(v) => Self::Float(v),
            EpicsValue::Enum(v) => Self::Enum(v),
            EpicsValue::Char(v) => Self::Char(v),
            EpicsValue::Long(v) => Self::Long(v),
            EpicsValue::Double(v) => Self::Double(v),
            EpicsValue::Int64(v) => Self::Int64(v),
            EpicsValue::UInt64(v) => Self::UInt64(v),
            EpicsValue::UShort(v) => Self::UShort(v),
            EpicsValue::ULong(v) => Self::ULong(v),
            EpicsValue::UChar(v) => Self::UChar(v),
            _ => return None,
        })
    }

    fn value(self) -> EpicsValue {
        match self {
            Self::Short(v) => EpicsValue::Short(v),
            Self::Float(v) => EpicsValue::Float(v),
            Self::Enum(v) => EpicsValue::Enum(v),
            Self::Char(v) => EpicsValue::Char(v),
            Self::Long(v) => EpicsValue::Long(v),
            Self::Double(v) => EpicsValue::Double(v),
            Self::Int64(v) => EpicsValue::Int64(v),
            Self::UInt64(v) => EpicsValue::UInt64(v),
            Self::UShort(v) => EpicsValue::UShort(v),
            Self::ULong(v) => EpicsValue::ULong(v),
            Self::UChar(v) => EpicsValue::UChar(v),
        }
    }
}

/// What a [`PostLog`] holds of the field's value (`db_field_log.h:112-132`).
#[derive(Debug, Clone)]
pub(crate) enum LogData {
    /// `dbfl_type_val`: a scalar copied into the log.
    Val(NativeValue),
    /// `dbfl_type_ref` with `dtor == NULL`: nothing copied, the consumer
    /// reads the live field at delivery.
    Ref,
    /// `dbfl_type_ref` with a `dtor`: a copy a filter made (`arr`), freed
    /// with the log.
    Owned(Box<EpicsValue>),
}

impl LogData {
    /// `value` held by the log: by value when it fits `native_value`, else
    /// the copy.
    fn holding(value: EpicsValue) -> Self {
        match NativeValue::of(&value) {
            Some(v) => Self::Val(v),
            None => Self::Owned(Box::new(value)),
        }
    }

    pub(crate) fn is_ref(&self) -> bool {
        matches!(self, Self::Ref)
    }
}

/// C `DB_AMSG_SIZE`: the message and its NUL.
const AMSG_SIZE: usize = 40;

/// The poster's side of a field log: what `db_create_field_log` copies off
/// the record (`dbEvent.c:704-713`), borrowed until the log copies it.
#[derive(Debug, Clone, Copy)]
pub(crate) struct LogHead {
    pub(crate) stat: u16,
    pub(crate) sevr: u16,
    /// Cut once per post, not once per subscriber.
    pub(crate) amsg: [u8; AMSG_SIZE],
    pub(crate) time: WallTime,
    pub(crate) utag: i32,
}

/// `amsg` as a log holds it: NUL-terminated, cut to `AMSG_SIZE - 1` bytes on
/// a char boundary (`strncpy` + `pbuffer[DB_AMSG_SIZE-1] = '\0'`,
/// `dbAccess.c:368-373`). A prefix, a middle and a suffix, as
/// [`SparseText`](crate::server::record::SparseText) holds a link failure's
/// `"field %s"` or `"%s Disconn"` without rendering it.
pub(crate) fn log_amsg(
    (prefix, text, suffix): (&'static str, &str, &'static str),
) -> [u8; AMSG_SIZE] {
    let mut amsg = [0u8; AMSG_SIZE];
    // C copies the rendered amsg once per log. The prefix is SparseText's
    // short ASCII constant, stored bytewise, so `text` is the one copy, and
    // an empty one is none.
    let mut at = 0;
    for (slot, byte) in amsg[..AMSG_SIZE - 1].iter_mut().zip(prefix.bytes()) {
        *slot = byte;
        at += 1;
    }
    let mut n = text.len().min(AMSG_SIZE - 1 - at);
    while !text.is_char_boundary(n) {
        n -= 1;
    }
    if n != 0 {
        amsg[at..at + n].copy_from_slice(&text.as_bytes()[..n]);
        at += n;
    }
    for (slot, byte) in amsg[at..AMSG_SIZE - 1].iter_mut().zip(suffix.bytes()) {
        *slot = byte;
    }
    amsg
}

/// [`log_amsg`] of a record's `amsg`, which almost every post finds absent:
/// only that test is inline, as in `SparseText`'s drop. Inline, the
/// three-part render merged into the absent path at every post.
#[inline(always)]
pub(crate) fn log_record_amsg(amsg: &crate::server::record::SparseText) -> [u8; AMSG_SIZE] {
    if amsg.is_empty() {
        return [0; AMSG_SIZE];
    }
    log_present_amsg(amsg)
}

#[cold]
#[inline(never)]
fn log_present_amsg(amsg: &crate::server::record::SparseText) -> [u8; AMSG_SIZE] {
    log_amsg(amsg.parts())
}

impl LogHead {
    pub(crate) fn of(snapshot: &Snapshot) -> Self {
        Self {
            stat: snapshot.alarm.status,
            sevr: snapshot.alarm.severity,
            amsg: log_amsg(("", &snapshot.alarm.amsg, "")),
            time: snapshot.timestamp,
            utag: snapshot.user_tag,
        }
    }
}

/// C `db_field_log` as `db_create_event_log` fills it (`dbEvent.c:702-730`):
/// fixed size, so an event queue holds it in its own ring and a post copies
/// into storage made at attach.
#[derive(Debug, Clone)]
pub struct PostLog {
    stat: u16,
    sevr: u16,
    /// As [`log_amsg`] cut it.
    amsg: [u8; AMSG_SIZE],
    time: WallTime,
    utag: i32,
    /// The `DBE_*` class(es) this post carries FOR THIS SUBSCRIBER — the
    /// poster's mask intersected with the subscriber's `select`
    /// (`pLog->mask = caEventMask & pevent->select`, `dbEvent.c:896-900`),
    /// produced by `Subscriber::delivered_mask`, the delivery gate, so the
    /// two cannot drift apart. When a post replaces a queued one the masks
    /// OR (see the event queue's documented deviations).
    mask: crate::server::recgbl::EventMask,
    /// Origin writer id; a subscription with the same `ignore_origin`
    /// skips it. Tagged by the `put_*_post` tier and inherited inside a
    /// `put_*_process` cascade through `AmbientWriteOriginScope`; 0 outside.
    origin: u64,
    pub(crate) value: LogData,
}

impl PostLog {
    fn new(
        head: LogHead,
        value: LogData,
        origin: u64,
        mask: crate::server::recgbl::EventMask,
    ) -> Self {
        Self {
            stat: head.stat,
            sevr: head.sevr,
            amsg: head.amsg,
            time: head.time,
            utag: head.utag,
            mask,
            origin,
            value,
        }
    }

    pub(crate) fn mask(&self) -> crate::server::recgbl::EventMask {
        self.mask
    }

    pub(crate) fn value(&self) -> &LogData {
        &self.value
    }

    /// Fold in the classes of a post this log absorbed.
    pub(crate) fn set_mask_or(&mut self, mask: crate::server::recgbl::EventMask) {
        self.mask |= mask;
    }
}

/// A log not yet made: what a post with no pre chain hands the event queue,
/// which builds the [`PostLog`] in the ring slot it lands in.
pub(crate) struct LogParts<'h> {
    pub(crate) head: &'h LogHead,
    pub(crate) value: LogData,
    pub(crate) origin: u64,
    pub(crate) mask: crate::server::recgbl::EventMask,
}

impl LogParts<'_> {
    #[inline]
    pub(crate) fn into_log(self) -> PostLog {
        PostLog::new(*self.head, self.value, self.origin, self.mask)
    }
}

/// One monitor update as a consumer takes it off the queue: a [`PostLog`].
#[derive(Debug, Clone)]
pub struct MonitorEvent {
    log: PostLog,
}

/// A monitor event's value, as C's `db_field_log` holds it
/// (`db_field_log.h:112-132`).
#[derive(Debug, Clone, PartialEq)]
pub enum LogValue<'a> {
    /// A scalar no wider than C's `union native_value`, by value
    /// (`dbfl_type_val`).
    Val(EpicsValue),
    /// A copy the log holds: a filter's (`arr`), or a consumer's read.
    Owned(&'a EpicsValue),
    /// No copy: read the live field at delivery (`dbfl_type_ref`).
    Ref,
}

impl LogValue<'_> {
    /// The value the event holds, `None` for a field read at delivery.
    pub fn as_value(&self) -> Option<&EpicsValue> {
        match self {
            LogValue::Val(v) => Some(v),
            LogValue::Owned(v) => Some(v),
            LogValue::Ref => None,
        }
    }
}

impl MonitorEvent {
    /// The log a producer posts for `snapshot`: its value copied only when
    /// it fits `native_value`, else read at delivery (`dbEvent.c:719-730`).
    /// Allocates nothing.
    pub fn post_log(
        snapshot: &Snapshot,
        origin: u64,
        mask: crate::server::recgbl::EventMask,
    ) -> Self {
        Self::posted(LogHead::of(snapshot), &snapshot.value, origin, mask)
    }

    /// [`Self::post_log`] of `head` and `value`.
    pub(crate) fn posted(
        head: LogHead,
        value: &EpicsValue,
        origin: u64,
        mask: crate::server::recgbl::EventMask,
    ) -> Self {
        let value = NativeValue::of(value).map_or(LogData::Ref, LogData::Val);
        Self {
            log: PostLog::new(head, value, origin, mask),
        }
    }

    pub(crate) fn into_log(self) -> PostLog {
        self.log
    }

    pub(crate) fn of_log(log: PostLog) -> Self {
        Self { log }
    }

    /// The posted value.
    pub fn value(&self) -> LogValue<'_> {
        match &self.log.value {
            LogData::Val(v) => LogValue::Val(v.value()),
            LogData::Owned(v) => LogValue::Owned(v),
            LogData::Ref => LogValue::Ref,
        }
    }

    pub fn stat(&self) -> u16 {
        self.log.stat
    }

    pub fn sevr(&self) -> u16 {
        self.log.sevr
    }

    pub fn amsg(&self) -> &str {
        let n = self
            .log
            .amsg
            .iter()
            .position(|&b| b == 0)
            .unwrap_or(AMSG_SIZE);
        // `log_amsg` cut on a char boundary.
        std::str::from_utf8(&self.log.amsg[..n]).unwrap_or_default()
    }

    pub fn time(&self) -> WallTime {
        self.log.time
    }

    pub fn utag(&self) -> i32 {
        self.log.utag
    }

    pub fn mask(&self) -> crate::server::recgbl::EventMask {
        self.log.mask
    }

    pub fn origin(&self) -> u64 {
        self.log.origin
    }

    /// Pre-chain filters rewrite their subscriber's log in place
    /// (`dbChannelRunPreChain`, `dbEvent.c:901`).
    pub fn set_time(&mut self, time: WallTime) {
        self.log.time = time;
    }

    pub fn set_utag(&mut self, utag: i32) {
        self.log.utag = utag;
    }

    pub fn set_mask(&mut self, mask: crate::server::recgbl::EventMask) {
        self.log.mask = mask;
    }

    /// Swap in `value`, handing back the one it replaces. The log must hold
    /// a value: a consumer gives a read-at-delivery log its read first
    /// ([`Self::set_value`]).
    pub fn replace_value(&mut self, value: EpicsValue) -> EpicsValue {
        match std::mem::replace(&mut self.log.value, LogData::holding(value)) {
            LogData::Val(v) => v.value(),
            LogData::Owned(v) => *v,
            LogData::Ref => {
                debug_assert!(false, "replace_value on a log read at delivery");
                EpicsValue::Long(0)
            }
        }
    }

    /// Give the log `value`, a read of the field made at delivery.
    pub fn set_value(&mut self, value: EpicsValue) {
        self.log.value = LogData::holding(value);
    }

    /// The value a consumer of `record`'s `field` delivers for this log: its
    /// own, or for a log that holds none the field read now, as
    /// `dbChannelGet` reads a `dbfl_type_ref` log (`dbChannel.h`
    /// `dbfl_pfield`). `None` when the field cannot be read.
    pub fn delivered_value(
        &self,
        record: &crate::server::record::RecordInstance,
        field: &str,
    ) -> Option<EpicsValue> {
        match self.value() {
            LogValue::Val(v) => Some(v),
            LogValue::Owned(v) => Some(v.clone()),
            LogValue::Ref => record.channel_field_value(field, false),
        }
    }

    /// C `db_create_read_log` filled from `read`: the log a GET's filter
    /// chain runs on (`dbChannelGet`), handed back to the read by
    /// [`Self::write_log_over`]. The value moves out of `read` until then.
    pub fn read_log(read: &mut Snapshot, mask: crate::server::recgbl::EventMask) -> Self {
        let value = LogData::holding(std::mem::replace(&mut read.value, EpicsValue::Long(0)));
        Self {
            log: PostLog::new(LogHead::of(read), value, 0, mask),
        }
    }

    /// Append this log's DBR `dbr_type` wire body to `dst` — `dbGet` into
    /// the reserved reply space (`dbAccess.c:1017-1018`) — for a class that
    /// reads no option ([`crate::types::encode_log_dbr_into`]). A log with
    /// no value of its own fails, as a `dbGet` with no field would.
    pub fn encode_dbr_into(&self, dst: &mut Vec<u8>, dbr_type: u16) -> crate::error::CaResult<()> {
        let value = self.value();
        let Some(value) = value.as_value() else {
            return Err(crate::error::CaError::GetConvertFailed(
                "a field log read at delivery has no value of its own".into(),
            ));
        };
        crate::types::encode_log_dbr_into(
            dst,
            dbr_type,
            value,
            self.stat(),
            self.sevr(),
            self.time(),
        )
    }

    /// Write this log's value, alarm, time and utag over `base`, a read of
    /// the channel made at delivery; every other option, ACKT/ACKS included,
    /// stays `base`'s (`getOptions`, `dbAccess.c:336-414`), narrowed to the
    /// log's type as a `ts` filter's `pfl->field_type` narrows it
    /// (`dbAccess.c:341-348,387-394`). A log read at delivery keeps `base`'s
    /// value.
    pub fn write_log_over(&self, base: &mut Snapshot) {
        if let Some(v) = self.value().as_value() {
            base.properties = base.properties.narrowed_to_field(v.db_field_type(), false);
            base.value = v.clone();
        }
        base.alarm.status = self.stat();
        base.alarm.severity = self.sevr();
        self.amsg().clone_into(&mut base.alarm.amsg);
        base.timestamp = self.time();
        base.user_tag = self.utag();
    }
}

/// A subscriber waiting for PV value updates — C `evSubscrip`'s producer-side
/// view. Its pending events live in the shared event queue
/// ([`crate::server::event_queue`]), reached only through `sink`.
pub struct Subscriber {
    pub sid: u32,
    pub data_type: DbFieldType,
    pub mask: u16,
    /// Producer half of this monitor's slot in the circuit's event queue.
    /// `pub(crate)` so no code outside this crate can enqueue past the
    /// append-vs-replace rule the queue owns.
    pub(crate) sink: EventSink,
    /// Server-side channel filter chain (epics-base 3.15.7).
    /// Defaults to empty — every event passes unchanged. Populated
    /// by the subscription path when the channel name carries a
    /// `.{filter:opts}` JSON suffix (`dbnd`, `arr`, `ts`, ...). Private:
    /// its filters are `Arc`s, and a reader of a shared entry must not be
    /// able to clone one and become its last owner.
    filters: crate::server::database::filters::FilterChain,
    /// Delivery gate. `true` (the default) delivers events normally;
    /// `false` suppresses every post to this subscriber at the source —
    /// nothing reaches the event queue, no filter is evaluated — so a
    /// paused monitor stops the record-event work entirely, not just the
    /// downstream frame. Mirrors EPICS `db_event_disable` / pvxs
    /// `onStart(false)` (singlesource.cpp:151-173, groupsource.cpp:151-281):
    /// the subscription object survives, only its event flow is gated, so
    /// the same subscriber resumes on re-enable. Atomic so a rights change
    /// flips it in a shared entry, never by replacing the entry.
    active: std::sync::atomic::AtomicBool,
}

impl Subscriber {
    /// An enabled monitor with no filters, posting into `sink`.
    pub(crate) fn new(sid: u32, data_type: DbFieldType, mask: u16, sink: EventSink) -> Self {
        Self {
            sid,
            data_type,
            mask,
            sink,
            filters: crate::server::database::filters::FilterChain::new(),
            active: std::sync::atomic::AtomicBool::new(true),
        }
    }

    /// Whether this monitor runs a channel-filter chain.
    pub fn has_filters(&self) -> bool {
        !self.filters.is_empty()
    }

    /// Append `filter` to this monitor's chain, before it is shared.
    pub(crate) fn push_filter(
        &mut self,
        filter: Arc<dyn crate::server::database::filters::SubscriptionFilter>,
    ) {
        self.filters.push(filter);
    }

    /// Install `filters` as this monitor's chain, before it is shared.
    pub(crate) fn set_filters(&mut self, filters: crate::server::database::filters::FilterChain) {
        self.filters = filters;
    }

    /// Whether posts reach this monitor (`db_event_enable` state).
    pub fn is_active(&self) -> bool {
        self.active.load(Ordering::Acquire)
    }

    /// C `db_event_enable` (`true`) / `db_event_disable` (`false`).
    pub(crate) fn set_active(&self, active: bool) {
        self.active.store(active, Ordering::Release);
    }

    /// The mask this subscriber's field log carries for `post`, or `None`
    /// when the post is not for it.
    ///
    /// C `db_post_events` evaluates `caEventMask & pevent->select` TWICE
    /// (`dbEvent.c:896-900`): once as the delivery gate, and again as the
    /// mask stamped on the log the pre-chain filters then see
    /// (`pLog->mask = caEventMask & pevent->select`). Returning the
    /// narrowed mask rather than a bool is what keeps the two inseparable
    /// — a caller cannot learn that it may deliver without also learning
    /// what mask to deliver under, so the wide poster mask can no longer
    /// leak past the gate into `dbnd`/`sync`/`dec`.
    ///
    /// An all-zero intersection means no delivery, which is also C: an
    /// empty `caEventMask` ands to zero and the poster loop skips the
    /// subscriber entirely.
    pub(crate) fn delivered_mask(
        &self,
        post: crate::server::recgbl::EventMask,
    ) -> Option<crate::server::recgbl::EventMask> {
        let narrowed = post & crate::server::recgbl::EventMask::from_bits(self.mask);
        (!narrowed.is_empty()).then_some(narrowed)
    }

    /// The single post path for both event sources (`ProcessVariable` value /
    /// alarm / property posts and `RecordInstance` field monitors): hand the
    /// event to this monitor's event queue, which owns C's append-vs-replace
    /// decision (`db_queue_event_log`), and apply the one piece of accounting
    /// that lives outside the queue — the counter for a value that a later post
    /// displaced before the consumer ever saw it (C `nreplace`, plus the
    /// latest-only collapse that C leaves uncounted; both mean one value the
    /// consumer will never see). The readers wake when `burst` drops.
    ///
    /// C `db_create_event_log` then `dbChannelRunPreChain` first: this
    /// monitor's log of `head` and `value` (`None` for a post that carries
    /// none), of the kind fixed at attach. With no pre chain the queue writes
    /// the log straight into its ring slot; a pre chain runs on the log as it
    /// is, copying nothing (`dbnd.c:75` reads only a `dbfl_type_val`,
    /// `sync.c:150`), and may drop it. The post chain's copies (`arr`, `ts`)
    /// are the consumer's.
    pub(crate) fn post(
        &self,
        burst: &mut PostBurst,
        head: &LogHead,
        value: Option<&EpicsValue>,
        origin: u64,
        mask: crate::server::recgbl::EventMask,
    ) {
        use crate::server::database::filters::FilteredMonitorEvent;
        let value = match (self.sink.kind(), value) {
            (crate::server::event_queue::LogKind::Val, Some(value)) => {
                NativeValue::of(value).map_or(LogData::Ref, LogData::Val)
            }
            _ => LogData::Ref,
        };
        let parts = LogParts {
            head,
            value,
            origin,
            mask,
        };
        let outcome = if self.filters.is_empty() {
            self.sink.post_parts(burst, parts)
        } else {
            let event = MonitorEvent::of_log(parts.into_log());
            let Some(fe) = self.filters.apply(FilteredMonitorEvent::new(event)) else {
                return;
            };
            self.sink.post_in(burst, fe.event)
        };
        if matches!(outcome, PostOutcome::Replaced | PostOutcome::Collapsed) {
            DROPPED_MONITOR_EVENTS.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// The consumer for this monitor is gone; the producer row can be reaped.
    pub(crate) fn is_closed(&self) -> bool {
        self.sink.is_closed()
    }

    /// This subscriber's slot, the key it is found by. Never `sid`: a client
    /// chooses that, and two circuits monitoring one PV can both choose it.
    pub fn id(&self) -> AttachId {
        self.sink.id()
    }

    /// C `db_post_single_event`: queue `snapshot` for this monitor alone,
    /// under its `select` (`db_create_event_log`) and through its pre-chain.
    /// `active` does not gate it; C posts before `db_event_enable` and
    /// before `db_event_disable`.
    pub(crate) fn post_single(&self, snapshot: Arc<Snapshot>) {
        let mask = crate::server::recgbl::EventMask::from_bits(self.mask);
        let head = LogHead::of(&snapshot);
        self.post(&mut PostBurst::new(), &head, Some(&snapshot.value), 0, mask);
    }
}

/// Monitors [`ProcessVariable::remove_subscriber`] took off the list. Its
/// drop ends each one's consumer stream; hold it past a lock to keep that
/// outside the lock.
pub struct RemovedMonitors(Vec<Arc<Subscriber>>);

impl Drop for RemovedMonitors {
    fn drop(&mut self) {
        // Not left to the last `Arc`: the map the removal retired holds one
        // until the list's next store.
        for sub in &self.0 {
            sub.sink.close();
        }
    }
}

impl RemovedMonitors {
    /// How many monitors were removed.
    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

/// Shadow `DBR_GR_*` / `DBR_CTRL_*` / enum metadata for a
/// non-record-backed PV.
///
/// A bare [`ProcessVariable`] has no record engine to derive units /
/// precision / display+alarm+control limits / enum labels from, so a
/// proxy that fronts an upstream IOC (the CA / PVA gateway) fetches the
/// upstream's control metadata and installs it here via
/// [`ProcessVariable::set_metadata`]. Every snapshot the PV emits —
/// the GET path ([`ProcessVariable::snapshot`]) and every monitor
/// event ([`ProcessVariable::post_property`], value, alarm, and
/// gateway snapshot posts) — then carries it, so a downstream client
/// that requested a `DBR_GR_*` / `DBR_CTRL_*` type receives the
/// upstream metadata instead of zeroed limits.
///
/// Mirrors the C ca-gateway, where `gatePvData` subscribes to
/// `DBE_PROPERTY` and issues a control-type `ca_array_get_callback`
/// (`gatePv.cc:850-934`) then copies units / precision / graphic +
/// control limits into the gateway's gdd attributes
/// (`gatePv.cc:1916-2007`).
#[derive(Debug, Clone, Default)]
pub struct PvMetadata {
    pub display: Option<DisplayInfo>,
    pub control: Option<ControlInfo>,
    pub enums: Option<EnumInfo>,
}

/// Metadata of the most recent full-snapshot write to a bare PV:
/// alarm + acquisition timestamp + userTag. A bare `ProcessVariable`
/// has no alarm engine, so without this it would forget everything a
/// full-value write carried beyond the raw value. pvxs mailbox
/// `SharedPV::post()` assigns the *whole* posted value to the current
/// value (`sharedpv.cpp:417-432`); to match that, a PV that received a
/// full posted Value must reflect its alarm/time on every later GET,
/// not just to the monitor subscribers that saw the post live.
#[derive(Clone)]
struct PostedMeta {
    alarm: crate::server::snapshot::AlarmInfo,
    timestamp: WallTime,
    user_tag: i32,
}

/// A process variable hosted by the server.
pub struct ProcessVariable {
    pub name: RecordName,
    /// The stored value. A synchronous `parking_lot::RwLock` (matching the
    /// sibling `posted_meta` / `metadata` / hook locks): every access is a
    /// single-expression read-or-write with no `.await` held across the
    /// guard, so the value-read path (`get`, `snapshot`) is pure lock work
    /// with no reactor dependency — the sans-io READ path. The write side
    /// (`set` / `set_snapshot`) still `.await`s the monitor fan-out, but
    /// drops this guard first.
    pub value: parking_lot::RwLock<EpicsValue>,
    /// Monitor fan-out list, C's `mlis` for this PV — every monitor under
    /// field 0. The post path walks a loaded map and takes no lock; only
    /// add, remove and [`Self::destroy`] take the list's writer mutex, from
    /// server threads, so a record-processing thread never waits on a
    /// `CAS-client` thread here.
    subscribers: SubscriberList<Subscriber>,
    /// Sticky metadata of the last full-snapshot write. `None` until a
    /// [`Self::set_snapshot`] lands; a value-only [`Self::set`] clears it
    /// back to `None` (a plain value write carries no explicit
    /// alarm/time, so it reverts to NO_ALARM + wall-clock-now). When
    /// `Some`, [`Self::snapshot`] serves these instead of the defaults.
    /// Single meaning: the served snapshot reflects the most recent
    /// write — value always current, metadata from that write.
    posted_meta: parking_lot::RwLock<Option<PostedMeta>>,
    /// Shadow DBR_GR_*/DBR_CTRL_*/enum metadata, installed by a proxy
    /// (CA / PVA gateway) via [`Self::set_metadata`]. Empty for a plain
    /// local PV. Stored under the same sync `parking_lot::RwLock` slot
    /// rationale as the hooks: every snapshot builder reads it without
    /// an `.await`. See [`PvMetadata`].
    metadata: parking_lot::RwLock<PvMetadata>,
    /// Optional hook consulted on client-originated writes. When set,
    /// the CA TCP write path delegates to the hook instead of doing a
    /// local `pv.set()`. See [`WriteHook`].
    ///
    /// Stored under `parking_lot::RwLock` (sync) rather than the
    /// async `tokio::sync::RwLock` so the hot put-path can read it
    /// without an `.await` round-trip — `write_hook()` is now a
    /// constant-time clone of the optional `Arc`. The hook itself
    /// is async (returns a `Future`); only the slot is sync.
    write_hook: parking_lot::RwLock<Option<WriteHook>>,
    /// optional access hook consulted by the CA server's
    /// `compute_access` to decide a downstream client's read/write
    /// rights for this PV. When set, it overrides the server's own ACF
    /// for this PV — the gateway uses it to enforce `.pvlist` ASG-based
    /// `can_read` / `can_write`, symmetric to [`Self::write_hook`].
    /// Same sync `parking_lot::RwLock` slot rationale as `write_hook`.
    access_hook: parking_lot::RwLock<Option<AccessHook>>,
    /// optional read hook consulted by the CA server's one-shot GET path
    /// ([`Self::read_snapshot`]) to fetch a fresh value instead of the
    /// stored cell. Used by the CA gateway's no-cache mode to forward
    /// each downstream GET to upstream. `None` (the default) keeps the
    /// read path serving the stored value, identical to before. Same
    /// sync slot rationale as [`Self::write_hook`]: the GET path clones
    /// the optional `Arc` without an `.await`, then awaits the hook
    /// outside any lock. See [`ReadHook`].
    read_hook: parking_lot::RwLock<Option<ReadHook>>,
}

impl ProcessVariable {
    pub fn new(name: impl Into<RecordName>, initial: EpicsValue) -> Self {
        Self {
            name: name.into(),
            value: parking_lot::RwLock::new(initial),
            subscribers: SubscriberList::default(),
            metadata: parking_lot::RwLock::new(PvMetadata::default()),
            posted_meta: parking_lot::RwLock::new(None),
            write_hook: parking_lot::RwLock::new(None),
            access_hook: parking_lot::RwLock::new(None),
            read_hook: parking_lot::RwLock::new(None),
        }
    }

    /// Install (or replace) the shadow DBR_GR_*/DBR_CTRL_*/enum
    /// metadata served on this PV's snapshots. Used by the CA / PVA
    /// gateway after fetching the upstream IOC's control metadata. See
    /// [`PvMetadata`]. To publish the change to downstream property
    /// monitors, follow with [`Self::post_property`].
    pub fn set_metadata(&self, metadata: PvMetadata) {
        *self.metadata.write() = metadata;
    }

    /// Snapshot (clone) of the installed shadow metadata; empty
    /// (`Default`) for a plain local PV.
    pub fn metadata(&self) -> PvMetadata {
        self.metadata.read().clone()
    }

    /// Fill any metadata field the snapshot leaves `None` from the
    /// installed shadow metadata. A field the caller already populated
    /// (e.g. a gateway snapshot that carried its own metadata) wins —
    /// this only supplies what is otherwise absent, so every emission
    /// path serves the upstream metadata uniformly without clobbering a
    /// richer source.
    fn apply_metadata(&self, snap: &mut Snapshot) {
        let meta = self.metadata.read();
        if snap.display.is_none() {
            snap.display = meta.display.clone();
        }
        if snap.control.is_none() {
            snap.control = meta.control.clone();
        }
        if snap.enums.is_none() {
            snap.enums = meta.enums.clone();
        }
        // A bare PV has no `rset`, so "which properties does this channel
        // supply" is answered by what metadata it actually HAS: a proxy that
        // shadowed an upstream IOC's display/control/enum info supplies those
        // properties, a mailbox PV that nobody gave metadata to supplies none.
        // Assigned here, in the one owner of a bare PV's metadata, so the mask
        // and the values it describes cannot disagree.
        // See [`crate::server::snapshot::PropertySupport`].
        snap.properties = PropertySupport {
            units: snap.display.is_some(),
            precision: snap.display.is_some(),
            graphic_double: snap.display.is_some(),
            alarm_double: snap.display.is_some(),
            control_double: snap.control.is_some(),
            enum_strs: snap.enums.is_some(),
            put_enum_str: None,
        }
        .narrowed_to_field(snap.value.db_field_type(), false);
    }

    /// Install an access hook. Replaces any previously
    /// installed hook.
    pub fn set_access_hook(&self, hook: AccessHook) {
        *self.access_hook.write() = Some(hook);
    }

    /// Snapshot of the installed access hook (clone of the `Arc`), or
    /// `None`. Consulted by the CA server's `compute_access`; cheap and
    /// non-async, like [`Self::write_hook`].
    pub fn access_hook(&self) -> Option<AccessHook> {
        self.access_hook.read().clone()
    }

    /// Install a read hook. Replaces any previously-installed hook.
    /// Used by the CA gateway's no-cache mode so each downstream GET is
    /// served by a fresh upstream fetch. See [`ReadHook`].
    pub fn set_read_hook(&self, hook: ReadHook) {
        *self.read_hook.write() = Some(hook);
    }

    /// Snapshot of the installed read hook (clone of the `Arc`), or
    /// `None`. Cheap and non-async, like [`Self::write_hook`]: the read
    /// lock is released before the cloned `Arc` returns, so the caller's
    /// subsequent `await` on the hook holds no lock.
    pub fn read_hook(&self) -> Option<ReadHook> {
        self.read_hook.read().clone()
    }

    /// Install a write hook. Replaces any previously-installed hook.
    pub fn set_write_hook(&self, hook: WriteHook) {
        *self.write_hook.write() = Some(hook);
    }

    /// Remove any installed write hook.
    pub fn clear_write_hook(&self) {
        *self.write_hook.write() = None;
    }

    /// Snapshot of the installed write hook (clone of the `Arc`), or
    /// `None` if none. Used by the CA TCP write path; cheap and
    /// non-async — the read lock is released before the cloned `Arc`
    /// returns, so the caller's subsequent `await` on the hook does
    /// not hold any lock.
    pub fn write_hook(&self) -> Option<WriteHook> {
        self.write_hook.read().clone()
    }

    /// Get the current value.
    ///
    /// Synchronous: a single-expression read-lock clone with no `.await`,
    /// so the value-read path carries no reactor dependency (sans-io).
    pub fn get(&self) -> EpicsValue {
        self.value.read().clone()
    }

    /// Build a Snapshot for this bare PV.
    ///
    /// A `ProcessVariable` is a non-record-backed channel: it has no
    /// alarm engine, no DESC/EGU/PREC metadata and no timestamp user
    /// tag of its own. The snapshot is therefore value + `NO_ALARM` +
    /// wall-clock now, with `user_tag` = 0. Display / control / enum
    /// metadata is `None` *unless* a proxy installed it via
    /// [`Self::set_metadata`] (the CA / PVA gateway shadowing an
    /// upstream IOC) — see `Self::apply_metadata`. Record-backed
    /// channels build their snapshot via
    /// `RecordInstance::snapshot_for_field`, which carries the record's
    /// own alarm/metadata. The only path that injects a non-zero alarm
    /// onto a bare PV is [`Self::post_alarm`] (used by the gateway
    /// adapter to surface upstream disconnect).
    pub fn snapshot(&self) -> Snapshot {
        self.snapshot_of(self.value.read().clone())
    }

    /// [`Self::snapshot`] around `value`, read by the caller.
    fn snapshot_of(&self, value: EpicsValue) -> Snapshot {
        // Serve the sticky metadata of the last full-snapshot write if
        // one landed (pvxs mailbox parity: a posted full Value stays the
        // current value, alarm/time included); otherwise the bare-PV
        // default of NO_ALARM + wall-clock-now.
        let mut snap = match self.posted_meta.read().clone() {
            Some(m) => {
                let mut s = Snapshot::new(value, m.alarm.status, m.alarm.severity, m.timestamp);
                s.alarm.ackt = m.alarm.ackt;
                s.alarm.acks = m.alarm.acks;
                s.user_tag = m.user_tag;
                s
            }
            None => Snapshot::new(value, 0, 0, crate::runtime::time::now_wall()),
        };
        self.apply_metadata(&mut snap);
        snap
    }

    /// `event` as a GET at delivery reads it: options from this PV now
    /// (`getOptions`, `dbAccess.c:336-414`), value, alarm, time and utag
    /// from the log.
    fn delivered(&self, event: &MonitorEvent) -> Snapshot {
        let mut snap = Snapshot::new(EpicsValue::Long(0), 0, 0, std::time::SystemTime::UNIX_EPOCH);
        if let Some(m) = self.posted_meta.read().as_ref() {
            snap.alarm.ackt = m.alarm.ackt;
            snap.alarm.acks = m.alarm.acks;
        }
        event.write_log_over(&mut snap);
        // Properties narrow to the value's type, so the log goes in first.
        self.apply_metadata(&mut snap);
        snap
    }

    /// Build the snapshot served on a one-shot client GET
    /// (`CA_PROTO_READ` / `CA_PROTO_READ_NOTIFY`).
    ///
    /// When a [`ReadHook`] is installed (the CA gateway's no-cache mode),
    /// the snapshot is fetched fresh through the hook — value *and* its
    /// upstream alarm status/severity and IOC timestamp together — and the
    /// shadow's last-known property metadata (display/control/enum) is
    /// overlaid for the fields a `DBR_TIME_*` event does not carry; on hook
    /// error the failure propagates so the server can answer `ECA_GETFAIL`,
    /// matching C ca-gateway forwarding each read to the IOC under
    /// `-no_cache` (`gateVc.cc:1361-1369`, `gatePv.cc:976`/`:1789-1794`).
    /// Without a hook this is exactly [`Self::snapshot`] wrapped in `Ok`,
    /// so the GET path is unchanged for every record-backed and cached PV.
    ///
    /// Only the GET path calls this; monitor fan-out, the initial monitor
    /// event, and access-rights re-posts keep using [`Self::snapshot`]
    /// (the stored value), so a no-cache PV still backs a downstream
    /// monitor with its upstream subscription's events rather than a
    /// per-event upstream get.
    pub async fn read_snapshot(&self) -> Result<Snapshot, CaError> {
        match self.read_hook() {
            Some(hook) => {
                // The hook issues a metadata-bearing upstream GET
                // (`DbrClass::Time`), so the returned snapshot already
                // carries the fresh value WITH its upstream alarm
                // status/severity and IOC timestamp — mirroring C
                // `getTimeCB` decoding the `DBR_TIME_*` event before
                // `setEventData`. A `DBR_TIME_*` event does not carry
                // display/control/enum metadata, so overlay the shadow's
                // last-known property metadata for those absent fields only
                // (a separate upstream path feeds it, exactly as C splits
                // the value/time path from the property monitor). Never
                // graft the fresh value onto the stored snapshot's
                // alarm/time, which may be stale or the bare-PV default.
                let mut snap = hook().await?;
                self.apply_metadata(&mut snap);
                Ok(snap)
            }
            None => Ok(self.snapshot()),
        }
    }

    /// Synchronous companion to [`Self::read_snapshot`] for the one-shot GET
    /// path (`CA_PROTO_READ` / `CA_PROTO_READ_NOTIFY`).
    ///
    /// `Some(snapshot)` when NO read hook is installed — the sans-io GET that
    /// every record-backed and cached PV takes: [`Self::snapshot`] of the
    /// stored value, produced with no `.await` and no reactor dependency.
    /// `None` when a gateway no-cache [`ReadHook`] IS installed, whose `hook()`
    /// is a genuine upstream network GET; the caller must then take the async
    /// [`Self::read_snapshot`] instead. This keeps the hook / no-hook decision
    /// in one owner, in lockstep with `read_snapshot` — the only difference is
    /// that the async fallible upstream fetch is surfaced to the caller as
    /// `None` rather than performed here.
    pub fn read_snapshot_local(&self) -> Option<Snapshot> {
        match self.read_hook() {
            Some(_) => None,
            None => Some(self.snapshot()),
        }
    }

    /// Set a new value and notify all subscribers.
    pub fn set(&self, new_value: EpicsValue) {
        self.set_with_origin(new_value, 0);
    }

    /// [`Self::set`] tagged with the writer's origin: the value post carries
    /// `origin` so an origin-aware consumer can recognise (and skip) the
    /// writer's own event — the simple-PV side of the
    /// `put_pv_and_post_with_origin` self-write contract. Origin 0 is the
    /// untagged default (never filtered).
    pub fn set_with_origin(&self, new_value: EpicsValue, origin: u64) {
        {
            let mut val = self.value.write();
            *val = new_value.clone();
        }
        // A plain value write carries no explicit alarm/time — revert to
        // the bare-PV default so a stale full-snapshot's metadata does
        // not linger on a value the client never stamped.
        *self.posted_meta.write() = None;
        self.notify_subscribers(new_value, origin);
    }

    /// Set value from a full snapshot (value + alarm + timestamp) and notify
    /// all subscribers. Used by the CA gateway forwarding task to propagate
    /// the upstream alarm status/severity and IOC timestamp to downstream
    /// monitors. Mirrors `gateVcData::setEventData` + `vcPostEvent` in the
    /// C ca-gateway: the incoming `dbr_time_xxx` GDD carries all three fields.
    pub fn set_snapshot(&self, snapshot: Snapshot) {
        {
            let mut val = self.value.write();
            *val = snapshot.value.clone();
        }
        // Persist the posted alarm/time/userTag so a later GET reflects
        // the full posted value, not just the live monitor fan-out.
        *self.posted_meta.write() = Some(PostedMeta {
            alarm: snapshot.alarm.clone(),
            timestamp: snapshot.timestamp,
            user_tag: snapshot.user_tag,
        });
        self.notify_subscribers_from_snapshot(snapshot);
    }

    /// Single delivery owner: emit `snapshot` to every live subscriber
    /// whose `DBE_*` mask intersects `post`.
    ///
    /// Every emission path ([`Self::notify_subscribers`] value posts,
    /// [`Self::post_alarm`], [`Self::notify_subscribers_from_snapshot`]
    /// gateway posts, [`Self::post_property`]) routes through here so the
    /// mask gate (`caEventMask & pevent->select`, `dbEvent.c:896-900`),
    /// the per-subscriber channel-filter chain, and the slow-consumer
    /// coalesce-overflow accounting are applied identically — one event
    /// class differs per caller, nothing else. The snapshot is built once
    /// by the caller (one timestamp per logical event); each subscriber gets
    /// a fixed field log of it, a wide value read at delivery.
    fn deliver(&self, post: crate::server::recgbl::EventMask, snapshot: Snapshot, origin: u64) {
        // Same ambient-origin inheritance as the record funnels
        // (`notify_field_with_origin` / `notify_from_snapshot`): a post
        // carrying no origin of its own inherits the current thread's
        // ambient write origin, so a simple PV written from inside an
        // in-process writer's synchronous put cascade tags its event
        // with the writer's origin too. 0 outside any scope.
        let origin = if origin != 0 {
            origin
        } else {
            crate::server::record::ambient_write_origin()
        };
        let subs = self.subscribers.load();
        let mut burst = PostBurst::new();
        let mut head = None;
        for sub in subs.of(0) {
            // Paused subscribers (`db_event_disable`) receive nothing —
            // skip before any work so a disabled monitor stops the event
            // flow at the source.
            if !sub.is_active() {
                continue;
            }
            // Gate and narrow in one step: `Subscriber::delivered_mask`
            // owns C's twice-used `caEventMask & pevent->select`.
            let Some(mask) = sub.delivered_mask(post) else {
                continue;
            };
            // The channel-filter chain may suppress this event (e.g.
            // `dbnd` deadband not crossed); the event's mask tells value
            // filters whether to pass through (446e0d4a). The queue appends,
            // or replaces this monitor's last entry in place when it is in
            // flow control or nearly full.
            let head = head.get_or_insert_with(|| LogHead::of(&snapshot));
            sub.post(&mut burst, head, Some(&snapshot.value), origin, mask);
        }
    }

    /// Push a fresh monitor event holding the current value but with
    /// the supplied alarm severity/status. Used by the PVA / CA
    /// gateway adapter to surface upstream-disconnect to downstream
    /// monitor subscribers without dropping the simple PV (which
    /// would force every downstream client into ECA_DISCONN +
    /// reconnect storms when the upstream is just briefly
    /// unreachable). Mirrors gatePvData::death's "alarm-post"
    /// alternative discussed in the C++ ca-gateway audit.
    pub fn post_alarm(&self, severity: u16, status: u16) {
        use crate::server::recgbl::EventMask;
        let value = self.value.read().clone();
        let snapshot = Snapshot::new(value, status, severity, crate::runtime::time::now_wall());
        // ALARM|LOG so DBE_LOG (archiver) subscribers receive alarm events.
        self.deliver(EventMask::ALARM | EventMask::LOG, snapshot, 0);
    }

    /// Post a `DBE_PROPERTY` monitor event carrying the decoded upstream
    /// CTRL event `snapshot` — its value plus the upstream status /
    /// severity and timestamp — so downstream property-change monitors
    /// re-read the installed units / precision / limits / enum labels at
    /// delivery with the *upstream* alarm state.
    ///
    /// Used by the CA / PVA gateway when an upstream `DBE_PROPERTY` event
    /// fires (metadata changed) after it has refreshed the shadow PV via
    /// [`Self::set_metadata`]. The caller supplies the snapshot rather than
    /// this method synthesising one: C ca-gateway decodes the upstream
    /// `DBR_CTRL_*` callback and re-posts the value with `setStatSevr()`
    /// status/severity preserved (`gatePv.cc:2413-2438`,
    /// `runValueDataCB`), leaving the timestamp as the control DBR carries
    /// none — it must NOT be replaced with a fresh `NO_ALARM` /
    /// wall-clock-now snapshot just because metadata changed. Pass the
    /// timestamp the upstream value carried (the control event has none of
    /// its own); pass `status`/`severity` from the upstream CTRL payload.
    /// Property events are a distinct class from value/alarm: only
    /// `DBE_PROPERTY` subscribers receive them.
    pub async fn post_property(&self, snapshot: Snapshot) {
        use crate::server::recgbl::EventMask;
        self.deliver(EventMask::PROPERTY, snapshot, 0);
    }

    /// Notify all subscribers of a new value, tagged with the writer's
    /// `origin` (0 = untagged).
    fn notify_subscribers(&self, value: EpicsValue, origin: u64) {
        use crate::server::recgbl::EventMask;
        let snapshot = Snapshot::new(value, 0, 0, crate::runtime::time::now_wall());
        // VALUE|LOG so DBE_LOG (archiver) subscribers receive value events.
        self.deliver(EventMask::VALUE | EventMask::LOG, snapshot, origin);
    }

    /// Notify all subscribers using a pre-built Snapshot (value + alarm +
    /// timestamp). Used by `set_snapshot` to propagate the upstream alarm
    /// and IOC timestamp without synthesising a new zero-alarm local-time
    /// snapshot. A field log carries no metadata: the reader takes the
    /// installed metadata at delivery.
    fn notify_subscribers_from_snapshot(&self, snapshot: Snapshot) {
        use crate::server::recgbl::EventMask;
        // C gateway fires postEvent(VALUE|ALARM|LOG) for every
        // upstream event (gateVc.cc:374-376); match it so DBE_LOG
        // archivers and DBE_ALARM-only monitors receive gateway snapshot posts.
        self.deliver(
            EventMask::VALUE | EventMask::LOG | EventMask::ALARM,
            snapshot,
            0,
        );
    }

    /// Add an in-process subscriber, attached to an event queue of its own.
    ///
    /// C `db_add_event(ctx, ...)` puts a monitor on the queue chain of the
    /// `event_user` (client) that owns it. An in-process consumer is its own
    /// client, so it gets its own [`EventUser`] — nothing else shares its
    /// queue, and flow control (a CA circuit concept) never engages on it.
    /// The CA server, whose subscriptions DO share one circuit-wide queue, uses
    /// [`Self::add_subscriber_on`].
    pub fn add_subscriber(
        self: &Arc<Self>,
        sid: u32,
        data_type: DbFieldType,
        mask: u16,
    ) -> Result<EventReader, SubscribeRefused> {
        self.add_subscriber_on(&EventUser::new(), sid, data_type, mask, no_context())
    }

    /// Add a subscriber whose events are queued on `user`'s event queue —
    /// C `db_add_event` with the circuit's `event_user` as context. Every
    /// subscription on one CA circuit shares that queue, and therefore its
    /// `nDuplicates`: a duplicate queued for one of them releases the
    /// EVENTS_OFF drain for all of them (`dbEvent.c:947`).
    ///
    /// Refuses with [`SubscribeRefused::CapReached`] when the per-PV subscriber
    /// cap is reached (defends
    /// against a misbehaving client opening many MONITOR ops against one shared
    /// PV; the per-channel cap limits channels but not subscriber rows on a
    /// single PV). Operators override it via `EPICS_CAS_MAX_SUBSCRIBERS_PER_PV`.
    pub fn add_subscriber_on(
        self: &Arc<Self>,
        user: &EventUser,
        sid: u32,
        data_type: DbFieldType,
        mask: u16,
        context: &DeliveryContext,
    ) -> Result<EventReader, SubscribeRefused> {
        self.attach(user, sid, data_type, mask, context, FilterChain::new())
    }

    /// CA EVENT_ADD on this PV: [`Self::add_subscriber_on`] with the
    /// channel's `filters` in the monitor before it is published, so no post
    /// reaches it unfiltered, then C `db_post_single_event`'s first value
    /// (`camessage.c:1904`) through that chain. Pass a fresh chain per
    /// monitor: `dbnd`, `dec` and `sync` keep state.
    pub fn add_monitor_on(
        self: &Arc<Self>,
        user: &EventUser,
        sid: u32,
        data_type: DbFieldType,
        mask: u16,
        context: &DeliveryContext,
        filters: FilterChain,
    ) -> Result<EventReader, SubscribeRefused> {
        let reader = self.attach(user, sid, data_type, mask, context, filters)?;
        self.post_single_event(reader.id());
        Ok(reader)
    }

    fn attach(
        self: &Arc<Self>,
        user: &EventUser,
        sid: u32,
        data_type: DbFieldType,
        mask: u16,
        context: &DeliveryContext,
        filters: FilterChain,
    ) -> Result<EventReader, SubscribeRefused> {
        let cap = max_subscribers_per_pv();
        let kind = crate::server::event_queue::LogKind::of_value(&self.value.read());
        let (sink, reader) = crate::server::event_queue::attach_with(user, sid, kind, context);
        let mut sub = Subscriber::new(sid, data_type, mask, sink);
        sub.set_filters(filters);
        self.subscribers.try_add(
            0,
            Arc::new(sub),
            // A destroyed PV's list is closed: the list refuses before this.
            |subs| {
                if subs.len() >= cap {
                    Err(SubscribeRefused::CapReached)
                } else {
                    Ok(())
                }
            },
            || (),
        )?;
        let owner: Arc<dyn RowOwner> = self.clone();
        Ok(reader.owned_by(Arc::downgrade(&owner)))
    }

    /// C `db_post_single_event` (`dbEvent.c:914-927`) for the subscriber
    /// holding slot `id`. The value read hold spans the read and the enqueue,
    /// as C's dbScanLock does: a set waits for it, so the monitor's last
    /// value is never older than the PV's.
    pub fn post_single_event(&self, id: AttachId) {
        if let Some(sub) = self.subscribers.load().of(0).find(|s| s.id() == id) {
            let value = self.value.read();
            sub.post_single(Arc::new(self.snapshot_of(value.clone())));
        }
    }

    /// C `db_event_enable` (`true`) / `db_event_disable` (`false`) for the
    /// subscriber holding slot `id`; see
    /// [`RecordInstance::set_subscriber_active`](crate::server::record::record_instance::RecordInstance::set_subscriber_active).
    pub fn set_subscriber_active(&self, id: AttachId, active: bool) {
        if let Some(sub) = self.subscribers.load().of(0).find(|s| s.id() == id) {
            sub.set_active(active);
        }
    }

    /// Remove the subscriber holding slot `id` (its reader's
    /// [`EventReader::id`]). The removed monitor is returned: dropping it
    /// is its consumer's end of stream, and runs after the writer mutex is
    /// released.
    pub fn remove_subscriber(&self, id: AttachId) -> RemovedMonitors {
        RemovedMonitors(self.subscribers.retain(|_, s| s.id() != id))
    }

    /// The number of monitors on this PV, live or not yet reaped.
    pub fn subscriber_count(&self) -> usize {
        self.subscribers.load().len()
    }

    /// Destroy this PV: drop every monitor and refuse every future one.
    ///
    /// The `casPV` destruction ca-gateway performs with `delete vc` when an
    /// upstream channel dies (`gatePv.cc:601`) — the downstream monitors
    /// stop rather than receive one more frame. Dropping the [`Subscriber`]
    /// rows drops their producer halves, so each consumer observes
    /// end-of-stream instead of silence.
    ///
    /// Reserved to the database's removal funnels ([`crate::server::database::PvDatabase::remove_simple_pv`]),
    /// so *removed from the database* and *destroyed* cannot come apart.
    /// Returns `true` for the call that performed the transition.
    pub(crate) fn destroy(&self) -> bool {
        // Destroyed is the list's closed state: one writer section empties
        // it and refuses every later add; the monitors end after it.
        self.subscribers.close().map(RemovedMonitors).is_some()
    }

    /// Whether `Self::destroy` has run. A server holding an `Arc` to this
    /// PV reads it to learn that the channel it serves has to be torn down.
    pub fn is_destroyed(&self) -> bool {
        self.subscribers.is_closed()
    }

    /// A fresh, LIVE PV carrying this one's identity: same name, same
    /// write / access / read hooks, same shadow metadata, seeded with
    /// `initial`.
    ///
    /// The replacement a proxy installs when what it fronts comes back —
    /// ca-gateway builds a new `gateVcData` on the next exist-test after
    /// `gatePvData::death` deleted the old one (`gatePv.cc:601`). The
    /// destruction mark is deliberately NOT carried: the corpse stays a
    /// corpse, and the replacement is a different object that the retired
    /// channels never held, so no client can be handed the new PV without
    /// a fresh `CREATE_CHANNEL`.
    pub fn respawn(&self, initial: EpicsValue) -> Self {
        let fresh = Self::new(self.name.clone(), initial);
        if let Some(h) = self.write_hook() {
            fresh.set_write_hook(h);
        }
        if let Some(h) = self.access_hook() {
            fresh.set_access_hook(h);
        }
        if let Some(h) = self.read_hook() {
            fresh.set_read_hook(h);
        }
        fresh.set_metadata(self.metadata());
        fresh
    }
}

/// Subscriber-id source for in-process [`PvSubscription`] monitors on a
/// [`ProcessVariable`]. A `ProcessVariable`'s subscriber `Vec` is disjoint
/// from any `RecordInstance`'s, so this is independent of the record-side
/// allocator; it only has to stay unique among the simple-PV subscribers
/// competing for one PV. Seeded at 1_000_000 for the same reason the
/// record allocator is — keep in-process sids clear of the low,
/// client-assigned wire subscription ids the CA server also registers on
/// the same PV.
static NEXT_PV_SUB_SID: AtomicU32 = AtomicU32::new(1_000_000);

fn next_pv_sub_sid() -> u32 {
    NEXT_PV_SUB_SID.fetch_add(1, Ordering::Relaxed)
}

/// In-process value-change monitor on a simple [`ProcessVariable`], the
/// counterpart of the record-side `DbSubscription`.
///
/// The PUT path (`ProcessVariable::set` / `set_snapshot`) calls
/// `notify_subscribers`, which fans the new value out to every registered
/// subscriber, so a consumer holding a `PvSubscription` observes every
/// later PUT — not just the connect-time snapshot. This mirrors pvxs
/// `SharedPV::post()` delivering a cloned update to each stored subscriber
/// (`sharedpv.cpp:417-440`).
///
/// The handle owns its `Subscriber` slot: `Drop` removes it, so a dropped
/// consumer cannot leave a dead subscriber row in
/// `ProcessVariable.subscribers` — the same leak `DbSubscription`'s `Drop`
/// closes for records.
pub struct PvSubscription {
    reader: EventReader,
    pv: Arc<ProcessVariable>,
}

impl PvSubscription {
    /// Register a value-change monitor on `pv`, refused when the per-PV
    /// subscriber cap is reached. The caller emits the initial
    /// snapshot itself (pvxs `SharedPV::attach` posts the current value
    /// before storing the subscriber); registering the subscriber *before*
    /// reading that snapshot is the miss-free ordering — a PUT racing the
    /// two is then delivered through the stream rather than lost.
    pub async fn subscribe(pv: Arc<ProcessVariable>) -> Result<Self, SubscribeRefused> {
        use crate::server::recgbl::EventMask;
        // VALUE|LOG matches the record-side `DbSubscription` default so
        // simple-PV and record-backed monitors gate identically; a
        // pure-alarm `post_alarm` (ALARM|LOG) still intersects via LOG.
        let mask = (EventMask::VALUE | EventMask::LOG).bits();
        let sid = next_pv_sub_sid();
        // `data_type` is nominal for snapshot consumers: `deliver` ships
        // the full `Snapshot` and gates only on mask/filters, never on the
        // stored type — `DbSubscription` likewise registers as `Double`.
        let reader = pv.add_subscriber(sid, DbFieldType::Double, mask)?;
        Ok(Self { reader, pv })
    }

    /// Await the next value change as a full `Snapshot`. A consumer that falls
    /// behind sees the same thing a C monitor does: its earlier distinct queued
    /// updates, and then — once the queue ran short of room — a tail entry
    /// carrying the latest value, because further posts replaced that entry in
    /// place rather than appending (`db_queue_event_log`, `dbEvent.c:812-827`).
    pub async fn recv_snapshot(&mut self) -> Option<Snapshot> {
        let event = self.reader.recv().await?;
        Some(self.pv.delivered(&event))
    }

    /// Non-blocking [`Self::recv_snapshot`]. Delegates to
    /// [`EventReader::try_recv`] (`event_queue.rs:807`) — same queue, same
    /// EVENTS_OFF gate, no suspension.
    ///
    /// Lets a PVA monitor source that adapts this stream be polled from a
    /// blocking drain loop with no reactor present.
    pub fn try_recv_snapshot(&mut self) -> Result<Snapshot, TryRecvError> {
        let event = self.reader.try_recv()?;
        Ok(self.pv.delivered(&event))
    }

    /// Await the next change as the full [`MonitorEvent`] — snapshot plus the
    /// per-event `DBE_*` mask. The mask-carrying counterpart of
    /// [`recv_snapshot`](Self::recv_snapshot), matching
    /// `DbSubscription::recv_event` so a consumer can treat a simple-PV and a
    /// record subscription through one shape.
    pub async fn recv_event(&mut self) -> Option<MonitorEvent> {
        self.reader.recv().await
    }

    /// Non-blocking [`Self::recv_event`].
    pub fn try_recv_event(&mut self) -> Result<MonitorEvent, TryRecvError> {
        self.reader.try_recv()
    }
}

impl Drop for PvSubscription {
    /// Remove this monitor's row from the PV, on the dropping thread.
    ///
    /// C cancels a monitor synchronously on the caller's thread
    /// (`db_cancel_event`, dbEvent.c), and here that is reachable:
    /// [`ProcessVariable::remove_subscriber`] takes
    /// `ProcessVariable::subscribers`, whose writer is a plain mutex, so the removal
    /// needs no executor and is complete when `drop` returns.
    ///
    /// `DbSubscription::drop` defers the same work to the background
    /// executor because *its* `remove_subscriber` is behind the record's
    /// async `RwLock`, which sync `drop` cannot take. Copying the deferral
    /// here also copied a `Handle::try_current()` test that decided whether
    /// to do the removal at all, and that predicate answers a question
    /// nobody asked: it is false on every callback-band worker and on every
    /// blocking CA connection thread, so a monitor dropped there kept its
    /// row for the life of the IOC and every later `notify_subscribers`
    /// paid to build an event for a reader that was gone.
    fn drop(&mut self) {
        self.pv.remove_subscriber(self.reader.id());
    }
}

#[cfg(test)]
mod log_amsg_tests {
    use super::*;

    fn logged(parts: (&'static str, &str, &'static str)) -> String {
        let head = LogHead {
            stat: 0,
            sevr: 0,
            amsg: log_amsg(parts),
            time: WallTime::UNIX_EPOCH,
            utag: 0,
        };
        let mask = crate::server::recgbl::EventMask::VALUE;
        MonitorEvent::of_log(PostLog::new(head, LogData::Ref, 0, mask))
            .amsg()
            .to_owned()
    }

    /// The 39-byte cut lands where the per-subscriber copy put it.
    #[test]
    fn a_long_amsg_is_cut_to_39_bytes_on_a_char_boundary() {
        let a41 = "a".repeat(41);
        assert_eq!(logged(("", &a41, "")), a41[..39]);
        // `é` would take bytes 39 and 40: the cut drops it whole.
        let a38 = "a".repeat(38);
        assert_eq!(logged(("", &format!("{a38}é"), "")), a38);
        assert_eq!(logged(("", "", "")), "");
        let name = "N".repeat(40);
        assert_eq!(
            logged(("field ", &name, "")),
            format!("field {}", &name[..33])
        );
        assert_eq!(logged(("field ", "", "")), "field ");
        assert_eq!(logged(("", "INP", " Disconn")), "INP Disconn");
        let n35 = "N".repeat(35);
        assert_eq!(logged(("", &n35, " Disconn")), format!("{n35} Dis"));
    }
}

#[cfg(test)]
mod mask_gate_tests {
    use super::*;

    // CA DBE_* monitor mask bits (db_access.h).
    const DBE_VALUE: u16 = 1;
    const DBE_LOG: u16 = 2;
    const DBE_ALARM: u16 = 4;

    fn pv() -> Arc<ProcessVariable> {
        Arc::new(ProcessVariable::new("test:pv", EpicsValue::Double(0.0)))
    }

    /// A full-snapshot write must persist alarm + timestamp + userTag so
    /// a later `snapshot()` (the GET path) reflects them — not just the
    /// live monitor fan-out. A subsequent value-only `set()` carries no
    /// explicit metadata and must revert the snapshot to NO_ALARM.
    #[epics_macros_rs::epics_test]
    async fn set_snapshot_metadata_persists_then_value_set_clears() {
        let pv = pv();

        // 42 ns exact: a `SystemTime` rounds this to 0 on Windows, so the
        // round-trip is built from `WallTime` integers to actually exercise
        // sub-100 ns persistence through `PostedMeta`.
        let posted_time = WallTime::from_unix(1_600_000_000, 42);
        let mut snap = Snapshot::new(EpicsValue::Double(7.0), 3, 2, posted_time);
        snap.user_tag = 9;
        pv.set_snapshot(snap);

        let got = pv.snapshot();
        assert_eq!(got.value, EpicsValue::Double(7.0), "value persisted");
        assert_eq!(got.alarm.status, 3, "alarm.status persisted to GET");
        assert_eq!(got.alarm.severity, 2, "alarm.severity persisted to GET");
        assert_eq!(got.user_tag, 9, "userTag persisted to GET");
        assert_eq!(got.timestamp, posted_time, "timestamp persisted to GET");

        // A plain value write reverts to the bare-PV default.
        pv.set(EpicsValue::Double(8.0));
        let after = pv.snapshot();
        assert_eq!(after.value, EpicsValue::Double(8.0));
        assert_eq!(after.alarm.status, 0, "value set clears posted alarm");
        assert_eq!(after.alarm.severity, 0, "value set clears posted severity");
        assert_eq!(after.user_tag, 0, "value set clears posted userTag");
        assert_ne!(
            after.timestamp, posted_time,
            "value set must restamp the timestamp, not keep the posted one"
        );
    }

    /// a `DBE_ALARM`-only subscriber must not receive a plain
    /// value set, but must receive an alarm post.
    #[epics_macros_rs::epics_test]
    async fn alarm_only_subscriber_skips_value_post() {
        let pv = pv();
        let mut rx = pv
            .add_subscriber(1, DbFieldType::Double, DBE_ALARM)
            .expect("subscriber added");
        pv.set(EpicsValue::Double(1.0));
        assert!(
            rx.try_recv().is_err(),
            "DBE_ALARM-only subscriber must not receive a value post"
        );
        pv.post_alarm(2, 3);
        assert!(
            rx.try_recv().is_ok(),
            "DBE_ALARM subscriber must receive an alarm post"
        );
    }

    /// a `DBE_VALUE`-only subscriber must not receive a
    /// `post_alarm`, but must receive value sets.
    #[epics_macros_rs::epics_test]
    async fn value_only_subscriber_skips_alarm_post() {
        let pv = pv();
        let mut rx = pv
            .add_subscriber(1, DbFieldType::Double, DBE_VALUE)
            .expect("subscriber added");
        pv.post_alarm(2, 3);
        assert!(
            rx.try_recv().is_err(),
            "DBE_VALUE-only subscriber must not receive an alarm post"
        );
        pv.set(EpicsValue::Double(1.0));
        assert!(
            rx.try_recv().is_ok(),
            "DBE_VALUE subscriber must receive a value post"
        );
    }

    /// C `db_post_events` stamps the field log with `caEventMask &
    /// pevent->select` (`dbEvent.c:896-900`), not with the poster's mask.
    /// `set_snapshot` posts `VALUE|LOG|ALARM` (the gateway-parity class set
    /// at `notify_subscribers_from_snapshot`), so a `DBE_VALUE`-only
    /// subscriber must see `DBE_VALUE` alone on the delivered event.
    #[epics_macros_rs::epics_test]
    async fn delivered_mask_is_narrowed_to_the_subscriber_select() {
        use crate::server::recgbl::EventMask;
        let pv = pv();
        let mut rx = pv
            .add_subscriber(1, DbFieldType::Double, DBE_VALUE)
            .expect("subscriber added");
        pv.set_snapshot(snapshot());
        let ev = rx.try_recv().expect("value-class post delivered");
        assert_eq!(
            ev.mask(),
            EventMask::VALUE,
            "delivered mask must be post & select, not the poster's full class set"
        );

        // The same narrowing on the other side: an ALARM-only subscriber
        // hears the same post as DBE_ALARM alone.
        let mut rx_alarm = pv
            .add_subscriber(2, DbFieldType::Double, DBE_ALARM)
            .expect("subscriber added");
        pv.set_snapshot(snapshot());
        let ev = rx_alarm.try_recv().expect("alarm-class post delivered");
        assert_eq!(
            ev.mask(),
            EventMask::ALARM,
            "narrowed for an ALARM-only sub"
        );
    }

    /// The consequence the narrowing exists for: a `.{dbnd}` pre-chain
    /// filter passes an event unconditionally when the log mask carries a
    /// class other than `DBE_VALUE`/`DBE_LOG` (`dbnd.c:84`, `send =
    /// pfl->mask & ~(DBE_VALUE|DBE_LOG)`). Handing it the poster's
    /// `VALUE|LOG|ALARM` therefore let every sub-deadband update through on
    /// the `DBE_ALARM` bit the client never subscribed to, silently
    /// defeating the deadband.
    #[epics_macros_rs::epics_test]
    async fn dbnd_on_a_value_only_subscriber_is_not_bypassed_by_the_alarm_bit() {
        use crate::server::database::filters::parser::parse_filter_chain;
        let pv = pv();
        let mut rx = pv
            .attach(
                &EventUser::new(),
                1,
                DbFieldType::Double,
                DBE_VALUE,
                no_context(),
                parse_filter_chain(r#"{"dbnd":{"d":10}}"#),
            )
            .expect("subscriber added");

        // First event: `dbnd`'s baseline is NaN, so C's `delta > deadband`
        // is INF > 10 and it always passes.
        pv.set_snapshot(Snapshot::new(
            EpicsValue::Double(0.0),
            0,
            0,
            std::time::SystemTime::UNIX_EPOCH,
        ));
        assert!(
            rx.try_recv().is_ok(),
            "first event establishes the baseline"
        );

        // Second event moves 0 -> 6 with a MINOR alarm: inside the band, so
        // C drops it. The alarm class is not in this subscriber's select and
        // must not reach the filter.
        pv.set_snapshot(Snapshot::new(
            EpicsValue::Double(6.0),
            7,
            1,
            std::time::SystemTime::UNIX_EPOCH,
        ));
        assert!(
            rx.try_recv().is_err(),
            "sub-deadband update must stay dropped; the poster's DBE_ALARM \
             bit is not part of a DBE_VALUE-only subscription"
        );
    }

    // --- Regression: set_snapshot must reach DBE_LOG and DBE_ALARM-only subs ---

    fn snapshot() -> Snapshot {
        Snapshot::new(
            EpicsValue::Double(2.0),
            0,
            0,
            std::time::SystemTime::UNIX_EPOCH,
        )
    }

    /// A DBE_LOG (archiver) subscriber must receive a set_snapshot post.
    #[epics_macros_rs::epics_test]
    async fn log_subscriber_receives_snapshot_post() {
        let pv = pv();
        let mut rx = pv
            .add_subscriber(1, DbFieldType::Double, DBE_LOG)
            .expect("subscriber added");
        pv.set_snapshot(snapshot());
        assert!(
            rx.try_recv().is_ok(),
            "DBE_LOG subscriber must receive a set_snapshot post"
        );
    }

    /// A DBE_ALARM-only subscriber must receive a set_snapshot post.
    #[epics_macros_rs::epics_test]
    async fn alarm_only_subscriber_receives_snapshot_post() {
        let pv = pv();
        let mut rx = pv
            .add_subscriber(1, DbFieldType::Double, DBE_ALARM)
            .expect("subscriber added");
        pv.set_snapshot(snapshot());
        assert!(
            rx.try_recv().is_ok(),
            "DBE_ALARM-only subscriber must receive a set_snapshot post"
        );
    }

    /// A DBE_VALUE subscriber must still receive a set_snapshot post.
    #[epics_macros_rs::epics_test]
    async fn value_subscriber_receives_snapshot_post() {
        let pv = pv();
        let mut rx = pv
            .add_subscriber(1, DbFieldType::Double, DBE_VALUE)
            .expect("subscriber added");
        pv.set_snapshot(snapshot());
        assert!(
            rx.try_recv().is_ok(),
            "DBE_VALUE subscriber must receive a set_snapshot post"
        );
    }

    /// A `DBE_VALUE | DBE_ALARM` subscriber receives both event classes.
    #[epics_macros_rs::epics_test]
    async fn both_classes_receive_both_posts() {
        let pv = pv();
        let mut rx = pv
            .add_subscriber(1, DbFieldType::Double, DBE_VALUE | DBE_ALARM)
            .expect("subscriber added");
        pv.set(EpicsValue::Double(1.0));
        assert!(rx.try_recv().is_ok(), "value post delivered to VALUE|ALARM");
        pv.post_alarm(2, 3);
        assert!(rx.try_recv().is_ok(), "alarm post delivered to VALUE|ALARM");
    }

    /// A DBE_LOG-only subscriber (archiver) must receive both value
    /// events and alarm events.  Pre-fix: VALUE-only / ALARM-only post masks
    /// never intersected DBE_LOG(2), so archivers received silence.
    #[epics_macros_rs::epics_test]
    async fn br_r52_log_subscriber_receives_value_and_alarm_events() {
        const DBE_LOG: u16 = 2;
        let pv = pv();
        let mut rx = pv
            .add_subscriber(1, DbFieldType::Double, DBE_LOG)
            .expect("subscriber added");
        pv.set(EpicsValue::Double(1.0));
        assert!(
            rx.try_recv().is_ok(),
            "DBE_LOG subscriber must receive a value post"
        );
        pv.post_alarm(2, 3);
        assert!(
            rx.try_recv().is_ok(),
            "DBE_LOG subscriber must receive an alarm post"
        );
    }

    /// Every delivered event carries its post's `DBE_*` class — the
    /// per-event mask C attaches to the field log (`db_field_log.mask()`)
    /// and pvxs narrows monitor decoding with (`groupsource.cpp:331-337`).
    #[epics_macros_rs::epics_test]
    async fn monitor_event_carries_post_class_mask() {
        use crate::server::recgbl::EventMask;
        let pv = pv();
        let mut rx = pv
            .add_subscriber(1, DbFieldType::Double, DBE_VALUE | DBE_LOG | DBE_ALARM)
            .expect("subscriber added");
        pv.set(EpicsValue::Double(1.0));
        assert_eq!(
            rx.try_recv().expect("value event").mask(),
            EventMask::VALUE | EventMask::LOG,
            "value post carries VALUE|LOG"
        );
        pv.post_alarm(2, 3);
        assert_eq!(
            rx.try_recv().expect("alarm event").mask(),
            EventMask::ALARM | EventMask::LOG,
            "alarm post carries ALARM|LOG"
        );
    }

    /// When the queue runs short of room and a post replaces this monitor's
    /// last entry in place, the surviving entry's mask is the OR of the
    /// displaced event's class and its own: the displaced *value* is gone (C
    /// frees the field log), but a narrow consumer must still learn that an
    /// ALARM-class change happened inside the coalesced tail.
    #[epics_macros_rs::epics_test]
    async fn in_place_replacement_accumulates_event_class_masks() {
        use crate::server::event_queue::{event_que_size, events_per_que};
        use crate::server::recgbl::EventMask;
        let pv = Arc::new(ProcessVariable::new(
            "coalesce:mask",
            EpicsValue::Double(0.0),
        ));
        let mut reader = pv
            .add_subscriber(7, DbFieldType::Double, DBE_VALUE | DBE_LOG | DBE_ALARM)
            .expect("subscriber added");
        // Append VALUE|LOG posts until the ring space reaches the replace
        // threshold; from here every post overwrites the tail entry.
        let appended = event_que_size() - events_per_que();
        for i in 1..=appended {
            pv.set(EpicsValue::Double(i as f64));
        }
        // Replaces the tail: its class (ALARM|LOG) must not be lost.
        pv.post_alarm(2, 3);
        // Replaces it again with a value post — both classes fold into the
        // survivor.
        pv.set(EpicsValue::Double(99.0));

        let mut last = None;
        while let Ok(event) = reader.try_recv() {
            last = Some(event);
        }
        let delivered = last.expect("the tail entry is delivered");
        assert_eq!(
            delivered.value().as_value().and_then(EpicsValue::to_f64),
            Some(99.0),
            "the tail entry carries the newest value"
        );
        assert!(
            delivered
                .mask()
                .contains(EventMask::VALUE | EventMask::ALARM | EventMask::LOG),
            "the displaced alarm class survives in the delivered mask (got {:?})",
            delivered.mask()
        );
    }

    /// R8-22 (simple-PV path): a monitor whose queue runs out of room during a
    /// burst must receive its EARLIER DISTINCT queued updates and then a tail
    /// entry carrying the latest value — C `db_queue_event_log` replaces only
    /// `*pLastLog` (`dbEvent.c:812-827`) and leaves the earlier entries queued.
    ///
    /// The old primitive parked the newest value in a side coalesce slot, and
    /// the consumer, finding it set, discarded the ENTIRE queued backlog and
    /// delivered only that newest value — so a 200-post burst came out as a
    /// single event instead of {1..107, 200}.
    #[epics_macros_rs::epics_test]
    async fn r8_22_pv_burst_keeps_earlier_distinct_updates() {
        use crate::server::event_queue::{event_que_size, events_per_que};
        use std::time::Duration;
        let pv = Arc::new(ProcessVariable::new("coalesce:pv", EpicsValue::Double(0.0)));
        let mut sub = PvSubscription::subscribe(pv.clone())
            .await
            .expect("subscribe");
        // With nothing draining, the first `appended` posts take ring entries
        // and every later post replaces the tail entry in place.
        let appended = event_que_size() - events_per_que();
        let burst = appended + 92;
        for i in 1..=burst {
            pv.set(EpicsValue::Double(i as f64));
        }
        let mut seq = Vec::new();
        while let Ok(Some(snap)) =
            crate::runtime::task::timeout(Duration::from_millis(200), sub.recv_snapshot()).await
        {
            seq.push(snap.value.to_f64().expect("double value"));
        }
        let want: Vec<f64> = (1..appended)
            .map(|i| i as f64)
            .chain(std::iter::once(burst as f64))
            .collect();
        assert_eq!(
            seq, want,
            "burst delivery must be {{earlier distinct backlog…, coalesced tail}}"
        );
    }
}

#[cfg(test)]
mod metadata_tests {
    use super::*;

    fn meta() -> PvMetadata {
        PvMetadata {
            display: Some(DisplayInfo {
                units: "degC".into(),
                precision: 2,
                upper_disp_limit: 100.0,
                lower_disp_limit: -50.0,
                upper_alarm_limit: 90.0,
                upper_warning_limit: 80.0,
                lower_warning_limit: -20.0,
                lower_alarm_limit: -40.0,
                ..Default::default()
            }),
            control: Some(ControlInfo {
                upper_ctrl_limit: 95.0,
                lower_ctrl_limit: -45.0,
            }),
            enums: None,
        }
    }

    fn pv() -> Arc<ProcessVariable> {
        Arc::new(ProcessVariable::new("m:pv", EpicsValue::Double(1.0)))
    }

    /// `set_with_origin` tags the value event with the writer's origin,
    /// plain `set` stays untagged, and a plain `set` inside an
    /// `AmbientWriteOriginScope` inherits the scope's origin — the
    /// simple-PV side of the record funnels' inheritance rule.
    #[epics_macros_rs::epics_test]
    async fn set_with_origin_tags_the_value_event() {
        const DBE_VALUE: u16 = 1;
        let pv = pv();
        let mut rx = pv
            .add_subscriber(1, DbFieldType::Double, DBE_VALUE)
            .expect("subscriber added");

        pv.set(EpicsValue::Double(2.0));
        assert_eq!(rx.try_recv().expect("plain set posts").origin(), 0);

        pv.set_with_origin(EpicsValue::Double(3.0), 77);
        assert_eq!(rx.try_recv().expect("tagged set posts").origin(), 77);

        {
            let _scope = crate::server::record::ambient_write_origin_scope(88);
            pv.set(EpicsValue::Double(4.0));
        }
        assert_eq!(
            rx.try_recv().expect("ambient-scoped set posts").origin(),
            88,
            "an originless simple-PV post inside an ambient scope must inherit it"
        );
    }

    /// A bare PV serves no metadata until a proxy installs it; after
    /// `set_metadata`, the GET snapshot carries the shadow DBR_GR/DBR_CTRL.
    #[epics_macros_rs::epics_test]
    async fn set_metadata_serves_on_get_snapshot() {
        let pv = pv();
        assert!(
            pv.snapshot().display.is_none(),
            "bare PV must carry no metadata before install"
        );
        pv.set_metadata(meta());
        let snap = pv.snapshot();
        let d = snap.display.expect("display installed");
        assert_eq!(d.units, "degC");
        assert_eq!(d.precision, 2);
        assert_eq!(
            snap.control.expect("control installed").upper_ctrl_limit,
            95.0
        );
    }

    /// `PvSubscription::recv_snapshot` reads the PV's metadata when the
    /// event is read, not when it was posted.
    #[epics_macros_rs::epics_test]
    async fn a_queued_value_is_read_with_the_metadata_installed_since() {
        let pv = pv();
        let mut sub = PvSubscription::subscribe(pv.clone()).await.unwrap();
        pv.set(EpicsValue::Double(1.5));
        pv.set_metadata(meta());
        let snap = sub.try_recv_snapshot().expect("set posts the value");
        assert_eq!(snap.value, EpicsValue::Double(1.5));
        assert_eq!(snap.display.expect("installed display").units, "degC");
    }

    /// `post_property` reaches DBE_PROPERTY subscribers and not
    /// DBE_VALUE-only subscribers.
    #[epics_macros_rs::epics_test]
    async fn post_property_reaches_only_property_subscribers() {
        const DBE_VALUE: u16 = 1;
        const DBE_PROPERTY: u16 = 8;
        let pv = pv();
        pv.set_metadata(meta());
        let mut prop_rx = pv
            .add_subscriber(1, DbFieldType::Double, DBE_PROPERTY)
            .expect("subscriber added");
        let mut val_rx = pv
            .add_subscriber(2, DbFieldType::Double, DBE_VALUE)
            .expect("subscriber added");
        pv.post_property(Snapshot::new(
            EpicsValue::Double(1.0),
            0,
            0,
            std::time::SystemTime::UNIX_EPOCH,
        ))
        .await;
        prop_rx
            .try_recv()
            .expect("DBE_PROPERTY subscriber receives property post");
        assert!(
            val_rx.try_recv().is_err(),
            "DBE_VALUE-only subscriber must not receive a property post"
        );
    }

    /// A property post
    /// must carry the upstream CTRL event's status/severity and timestamp,
    /// not a fabricated `NO_ALARM` / wall-clock-now snapshot. C ca-gateway
    /// preserves `setStatSevr()` on the property callback
    /// (`gatePv.cc:2413-2438`); a downstream `DBE_PROPERTY` monitor must
    /// see `severity=MAJOR` and the upstream timestamp, even though only
    /// metadata changed.
    #[epics_macros_rs::epics_test]
    async fn post_property_preserves_upstream_alarm_and_timestamp() {
        const DBE_PROPERTY: u16 = 8;
        const MAJOR: u16 = 2; // epicsSevMajor
        const HIGH: u16 = 3; // epicsAlarmHigh
        let pv = pv();
        pv.set_metadata(meta());
        let mut prop_rx = pv
            .add_subscriber(1, DbFieldType::Double, DBE_PROPERTY)
            .expect("subscriber added");
        // The upstream CTRL event timestamp: a fixed point in the past, so
        // it is unmistakably NOT a fresh wall clock minted by the post.
        let upstream_ts = WallTime::from_unix(1_000_000, 0);
        pv.post_property(Snapshot::new(
            EpicsValue::Double(2.0),
            HIGH,
            MAJOR,
            upstream_ts,
        ))
        .await;
        let ev = prop_rx.try_recv().expect("property post delivered");
        assert_eq!(
            ev.sevr(),
            MAJOR,
            "property post must carry the upstream MAJOR severity, not NO_ALARM"
        );
        assert_eq!(ev.stat(), HIGH, "upstream status preserved");
        assert_eq!(
            ev.time(),
            upstream_ts,
            "property post must keep the upstream timestamp, not a fresh wall clock"
        );
    }
}

#[cfg(test)]
mod read_hook_tests {
    use super::*;

    fn pv() -> Arc<ProcessVariable> {
        Arc::new(ProcessVariable::new("g:pv", EpicsValue::Double(1.0)))
    }

    /// No hook installed (the default for every record-backed and cached
    /// PV): `read_snapshot` is exactly `snapshot` wrapped in `Ok` — the
    /// stored value, byte-for-byte unchanged.
    #[epics_macros_rs::epics_test]
    async fn read_snapshot_without_hook_equals_snapshot() {
        let pv = pv();
        let read = pv.read_snapshot().await.expect("no-hook read never errors");
        let stored = pv.snapshot();
        assert_eq!(read.value, stored.value);
        assert_eq!(read.value, EpicsValue::Double(1.0));
    }

    /// With a hook installed (no-cache mode), the GET value comes fresh
    /// from the hook, NOT from the stored shadow value — the stored value
    /// stays a stale sentinel that the hook overrides.
    #[epics_macros_rs::epics_test]
    async fn read_snapshot_fires_hook_for_fresh_value() {
        let pv = pv();
        // Stored shadow value is a sentinel the hook must override.
        pv.set(EpicsValue::Double(999.0));
        pv.set_read_hook(Arc::new(|| {
            Box::pin(async {
                Ok(Snapshot::new(
                    EpicsValue::Double(42.0),
                    0,
                    0,
                    std::time::UNIX_EPOCH,
                ))
            })
        }));
        let read = pv.read_snapshot().await.expect("hook returns Ok");
        assert_eq!(
            read.value,
            EpicsValue::Double(42.0),
            "GET must serve the hook's fresh value, not the stored sentinel"
        );
    }

    /// A hook failure propagates so the server can answer `ECA_GETFAIL`,
    /// matching C ca-gateway forwarding each read to the IOC.
    #[epics_macros_rs::epics_test]
    async fn read_snapshot_propagates_hook_error() {
        let pv = pv();
        pv.set_read_hook(Arc::new(|| Box::pin(async { Err(CaError::Disconnected) })));
        let err = pv.read_snapshot().await.expect_err("hook error propagates");
        assert!(matches!(err, CaError::Disconnected));
    }

    /// No hook (every record-backed and cached PV): the sync companion
    /// `read_snapshot_local` yields `Some(snapshot)`, byte-for-byte the same
    /// value as `snapshot` / the async `read_snapshot` — the fully sans-io
    /// GET path.
    #[test]
    fn read_snapshot_local_without_hook_is_some_and_matches_snapshot() {
        let pv = pv();
        let local = pv
            .read_snapshot_local()
            .expect("no hook ⇒ sync snapshot is Some");
        assert_eq!(local.value, pv.snapshot().value);
        assert_eq!(local.value, EpicsValue::Double(1.0));
    }

    /// A read hook installed (gateway no-cache): the sync companion returns
    /// `None`, the signal that the caller must take the async upstream-GET
    /// path — `read_snapshot_local` never fires the hook itself.
    #[test]
    fn read_snapshot_local_with_hook_is_none() {
        let pv = pv();
        pv.set_read_hook(Arc::new(|| {
            Box::pin(async {
                Ok(Snapshot::new(
                    EpicsValue::Double(42.0),
                    0,
                    0,
                    std::time::UNIX_EPOCH,
                ))
            })
        }));
        assert!(
            pv.read_snapshot_local().is_none(),
            "a read hook ⇒ the sync path defers to the async upstream GET"
        );
    }

    /// The read hook is GET-path only: `snapshot` (monitor fan-out, the
    /// initial monitor event, access-rights re-posts) keeps serving the
    /// stored value even when a hook is installed.
    #[epics_macros_rs::epics_test]
    async fn snapshot_ignores_read_hook() {
        let pv = pv();
        pv.set(EpicsValue::Double(7.0));
        pv.set_read_hook(Arc::new(|| {
            Box::pin(async {
                Ok(Snapshot::new(
                    EpicsValue::Double(42.0),
                    0,
                    0,
                    std::time::UNIX_EPOCH,
                ))
            })
        }));
        let snap = pv.snapshot();
        assert_eq!(
            snap.value,
            EpicsValue::Double(7.0),
            "snapshot must serve the stored value, never the read hook"
        );
    }

    /// Fresh value + upstream alarm/time ride from the hook; the shadow's
    /// installed *property* metadata (display/control/enum) — which a
    /// `DBR_TIME_*` event does not carry — is overlaid for those fields.
    #[epics_macros_rs::epics_test]
    async fn read_snapshot_carries_shadow_metadata() {
        let pv = pv();
        pv.set_metadata(PvMetadata {
            display: Some(DisplayInfo {
                units: "mm".into(),
                precision: 3,
                ..Default::default()
            }),
            control: None,
            enums: None,
        });
        // The hook returns a Time-class snapshot (value + alarm + time,
        // no display/control/enum), exactly as `get_with_metadata(Time)`.
        pv.set_read_hook(Arc::new(|| {
            Box::pin(async {
                Ok(Snapshot::new(
                    EpicsValue::Double(5.0),
                    0,
                    0,
                    std::time::UNIX_EPOCH,
                ))
            })
        }));
        let read = pv.read_snapshot().await.expect("hook returns Ok");
        assert_eq!(read.value, EpicsValue::Double(5.0));
        assert_eq!(
            read.display
                .expect("shadow property metadata rides fresh value")
                .units,
            "mm"
        );
    }

    /// A no-cache GET must report the FRESH upstream alarm and timestamp
    /// that travel with the value (C `getTimeCB` decodes the `DBR_TIME_*`
    /// event's status/severity/time before `setEventData`,
    /// `gatePv.cc:1789-1794`), NOT the shadow's last monitor-posted (or
    /// bare-PV default) alarm/time. Before the fix the read hook returned
    /// a bare value and `read_snapshot` grafted it onto the stored
    /// snapshot, so the GET reported the new value with a stale or default
    /// status/severity/timestamp.
    #[epics_macros_rs::epics_test]
    async fn read_snapshot_carries_upstream_alarm_not_shadow() {
        use std::time::{Duration, UNIX_EPOCH};
        let pv = pv();
        // The shadow's stored snapshot carries one alarm/time (a prior
        // monitor post). Make it concrete and DIFFERENT from the upstream
        // GET so a graft-onto-shadow regression is observable.
        let shadow_time = UNIX_EPOCH + Duration::from_secs(1_000);
        pv.set_snapshot(Snapshot::new(EpicsValue::Double(1.0), 7, 1, shadow_time));
        // The fresh upstream GET reports a different value, alarm, and time.
        let upstream_time = WallTime::from_unix(2_000, 0);
        pv.set_read_hook(Arc::new(move || {
            Box::pin(
                async move { Ok(Snapshot::new(EpicsValue::Double(5.0), 17, 2, upstream_time)) },
            )
        }));
        let read = pv.read_snapshot().await.expect("hook returns Ok");
        assert_eq!(read.value, EpicsValue::Double(5.0), "fresh upstream value");
        assert_eq!(
            read.alarm.status, 17,
            "upstream alarm status, not shadow's 7"
        );
        assert_eq!(read.alarm.severity, 2, "upstream severity, not shadow's 1");
        assert_eq!(
            read.timestamp, upstream_time,
            "upstream timestamp, not shadow's"
        );
    }
}

/// BR-3 — removal from the database IS destruction, and destruction stops
/// the monitors instead of handing them one more event.
///
/// C ca-gateway `gatePvData::death` deletes the downstream virtual channel
/// when its upstream dies (`delete vc; vc = NULL;`, gatePv.cc:600-601). The
/// tests below pin the two halves that make that expressible here: a
/// destroyed PV has no subscribers and can gain none, and `respawn` is the
/// only way back — onto a different object.
#[cfg(test)]
mod destruction_tests {
    use super::*;
    use crate::server::database::PvDatabase;
    use crate::server::snapshot::DisplayInfo;
    use crate::types::PvString;

    #[epics_macros_rs::epics_test]
    async fn removing_a_simple_pv_destroys_it_and_ends_its_monitors() {
        let db = PvDatabase::new();
        db.add_pv("D:pv", EpicsValue::Double(1.0))
            .await
            .expect("fresh name registers");
        let pv = db.find_pv("D:pv").expect("just registered");
        let mut reader = pv
            .add_subscriber(7, DbFieldType::Double, u16::MAX)
            .expect("first subscriber");

        let removed = db.remove_simple_pv("D:pv").await.expect("was registered");
        assert!(
            removed.is_destroyed(),
            "the removal funnel is `destroy`'s only caller, so removed => destroyed"
        );
        assert!(
            removed.subscriber_count() == 0,
            "destruction drops the subscriber rows"
        );
        assert!(
            matches!(reader.try_recv(), Err(TryRecvError::Disconnected)),
            "the consumer must observe end-of-stream, not silence"
        );
    }

    #[epics_macros_rs::epics_test]
    async fn a_destroyed_pv_refuses_a_new_monitor() {
        let db = PvDatabase::new();
        db.add_pv("D:refuse", EpicsValue::Double(1.0))
            .await
            .expect("fresh name registers");
        let removed = db
            .remove_simple_pv("D:refuse")
            .await
            .expect("was registered");
        assert!(
            removed
                .add_subscriber(1, DbFieldType::Double, u16::MAX)
                .is_err(),
            "a CREATE_CHAN + EVENT_ADD racing the removal must not re-attach"
        );
    }

    #[epics_macros_rs::epics_test]
    async fn respawn_carries_the_hooks_and_metadata_onto_a_live_pv() {
        let pv = ProcessVariable::new("D:respawn", EpicsValue::Double(1.0));
        pv.set_write_hook(Arc::new(|_v, _ctx| Box::pin(async { Ok(()) })));
        pv.set_access_hook(Arc::new(|_user, _host| AccessDecision {
            read: true,
            write: false,
        }));
        pv.set_read_hook(Arc::new(|| {
            Box::pin(async {
                Ok(Snapshot::new(
                    EpicsValue::Double(9.0),
                    0,
                    0,
                    std::time::SystemTime::UNIX_EPOCH,
                ))
            })
        }));
        let display = DisplayInfo {
            units: "mA".into(),
            ..Default::default()
        };
        pv.set_metadata(PvMetadata {
            display: Some(display),
            control: None,
            enums: None,
        });
        pv.destroy();

        let fresh = pv.respawn(EpicsValue::Double(5.0));
        assert!(!fresh.is_destroyed(), "the replacement is live");
        assert!(pv.is_destroyed(), "the corpse stays a corpse");
        assert!(fresh.write_hook().is_some(), "write hook carried");
        assert!(fresh.access_hook().is_some(), "access hook carried");
        assert!(fresh.read_hook().is_some(), "read hook carried");
        assert_eq!(
            fresh.metadata().display.expect("display carried").units,
            PvString::from("mA"),
            "shadow DBR_CTRL metadata carried, not zeroed"
        );
        assert_eq!(*fresh.value.read(), EpicsValue::Double(5.0));
    }

    #[epics_macros_rs::epics_test]
    async fn removing_a_record_destroys_it_too() {
        let db = PvDatabase::new();
        db.add_record(
            "D:rec",
            Box::new(crate::server::records::ai::AiRecord::default()),
        )
        .await
        .expect("ai record");
        let rec = db.get_record("D:rec").expect("just added");
        assert!(!rec.read().is_destroyed());
        assert!(db.remove_record("D:rec").await, "record was registered");
        assert!(
            rec.read().is_destroyed(),
            "the record funnel marks the instance a CA channel still holds"
        );
    }
}

#[cfg(test)]
mod subscription_drop_tests {
    use super::*;

    /// The thread a monitor is actually dropped on is not the thread it was
    /// created on. A blocking CA connection thread and a callback-band worker
    /// are both outside every tokio runtime, and that is where a circuit
    /// teardown releases its `PvSubscription`.
    #[epics_macros_rs::epics_test]
    async fn a_subscription_dropped_on_a_bare_thread_removes_its_row() {
        let pv = Arc::new(ProcessVariable::new("D:baredrop", EpicsValue::Double(0.0)));
        let sub = PvSubscription::subscribe(pv.clone())
            .await
            .expect("first subscriber");
        assert_eq!(pv.subscriber_count(), 1, "the monitor registered");

        let dropper = {
            let pv = pv.clone();
            std::thread::spawn(move || {
                assert!(
                    tokio::runtime::Handle::try_current().is_err(),
                    "the case is only meaningful off a runtime"
                );
                drop(sub);
                // Synchronous, so the row is gone before this thread joins —
                // no polling, no executor to wait for.
                assert!(
                    pv.subscriber_count() == 0,
                    "removal completes inside `drop`"
                );
            })
        };
        dropper.join().expect("the dropping thread must not panic");

        assert!(
            pv.subscriber_count() == 0,
            "a monitor dropped off a runtime must not leave its row behind"
        );
    }

    /// The other boundary: a drop removes one row, not the PV's whole
    /// subscriber list.
    #[epics_macros_rs::epics_test]
    async fn dropping_one_subscription_leaves_its_sibling_registered() {
        let pv = Arc::new(ProcessVariable::new("D:sibling", EpicsValue::Double(0.0)));
        let first = PvSubscription::subscribe(pv.clone())
            .await
            .expect("first subscriber");
        let second = PvSubscription::subscribe(pv.clone())
            .await
            .expect("second subscriber");
        assert_eq!(pv.subscriber_count(), 2, "both monitors registered");

        drop(first);
        assert_eq!(
            pv.subscriber_count(),
            1,
            "only the dropped monitor's row goes"
        );
        drop(second);
        assert!(pv.subscriber_count() == 0, "and then the other one");
    }
}

#[cfg(test)]
mod subscriber_list_consumer_tests {
    use super::*;
    use crate::server::database::filters::{FilteredMonitorEvent, SubscriptionFilter};
    use std::sync::Mutex as StdMutex;
    use std::sync::atomic::AtomicUsize;

    const DBE_VALUE: u16 = 1;

    fn pv() -> Arc<ProcessVariable> {
        Arc::new(ProcessVariable::new("P", EpicsValue::Double(0.0)))
    }

    /// Drops every event and counts what it saw.
    struct DropAll(Arc<AtomicUsize>);

    impl SubscriptionFilter for DropAll {
        fn name(&self) -> &'static str {
            "drop-all"
        }
        fn apply(&self, _: FilteredMonitorEvent) -> Option<FilteredMonitorEvent> {
            self.0.fetch_add(1, Ordering::Relaxed);
            None
        }
    }

    /// Racing a destroy, an add is refused or published and then removed by
    /// it, whose drop ends the add's stream: destroyed never has a monitor.
    #[test]
    fn an_add_racing_destroy_is_refused_or_ended() {
        for _ in 0..300 {
            let pv = pv();
            let added = std::thread::scope(|s| {
                let adder = s.spawn(|| pv.add_subscriber(1, DbFieldType::Double, DBE_VALUE));
                assert!(pv.destroy());
                adder.join().unwrap()
            });
            assert_eq!(pv.subscriber_count(), 0, "destroyed with a monitor");
            match added {
                Ok(mut reader) => assert!(
                    matches!(reader.try_recv(), Err(TryRecvError::Disconnected)),
                    "a removed monitor's stream did not end"
                ),
                Err(why) => assert_eq!(why, SubscribeRefused::NoRecord),
            }
        }
    }

    /// The chain is in the monitor before it is published: neither the seed
    /// nor a post racing the add reaches the queue unfiltered.
    #[test]
    fn a_post_racing_add_monitor_runs_its_filter() {
        let pv = pv();
        let seen = Arc::new(AtomicUsize::new(0));
        let stop = std::sync::atomic::AtomicBool::new(false);
        std::thread::scope(|s| {
            s.spawn(|| {
                let mut v = 0.0;
                while !stop.load(Ordering::Relaxed) {
                    v += 1.0;
                    pv.set(EpicsValue::Double(v));
                }
            });
            // Stops the poster on every exit, a failed assertion included.
            struct Stop<'a>(&'a std::sync::atomic::AtomicBool);
            impl Drop for Stop<'_> {
                fn drop(&mut self) {
                    self.0.store(true, Ordering::Relaxed);
                }
            }
            let _stop = Stop(&stop);
            let user = EventUser::new();
            for sid in 0..200 {
                let mut chain = FilterChain::new();
                chain.push(Arc::new(DropAll(seen.clone())));
                let mut reader = pv
                    .add_monitor_on(
                        &user,
                        sid,
                        DbFieldType::Double,
                        DBE_VALUE,
                        no_context(),
                        chain,
                    )
                    .expect("added");
                assert!(
                    matches!(reader.try_recv(), Err(TryRecvError::Empty)),
                    "monitor {sid} got an unfiltered event"
                );
                drop(pv.remove_subscriber(reader.id()));
            }
        });
        assert!(
            seen.load(Ordering::Relaxed) >= 200,
            "the seeds ran the chain"
        );
    }

    /// Parks the first event (the seed) while a `set` runs on another thread.
    struct ParkSeed(
        std::sync::Weak<ProcessVariable>,
        StdMutex<Option<std::thread::JoinHandle<()>>>,
    );

    impl SubscriptionFilter for ParkSeed {
        fn name(&self) -> &'static str {
            "park-seed"
        }
        fn apply(&self, ev: FilteredMonitorEvent) -> Option<FilteredMonitorEvent> {
            let mut setter = self.1.lock().unwrap();
            if setter.is_none() {
                let pv = self.0.upgrade().unwrap();
                let (tx, rx) = std::sync::mpsc::channel();
                *setter = Some(std::thread::spawn(move || {
                    pv.set(EpicsValue::Double(1.0));
                    // Gone once the seed stopped waiting.
                    let _ = tx.send(());
                }));
                drop(setter);
                let _ = rx.recv_timeout(std::time::Duration::from_millis(300));
            }
            Some(ev)
        }
    }

    /// A set racing EVENT_ADD's seed orders after it: the monitor's last value
    /// is the set's, as C's dbScanLock across create and queue gives
    /// (dbEvent.c:920-926). Without the hold the set's post is queued first
    /// and the consumer ends on the stale seed.
    #[test]
    fn a_set_racing_the_seed_is_delivered_after_it() {
        let pv = pv();
        let park = Arc::new(ParkSeed(Arc::downgrade(&pv), StdMutex::new(None)));
        let mut chain = FilterChain::new();
        chain.push(park.clone());
        let user = EventUser::new();
        let mut reader = pv
            .add_monitor_on(
                &user,
                1,
                DbFieldType::Double,
                DBE_VALUE,
                no_context(),
                chain,
            )
            .expect("added");
        let setter = park.1.lock().unwrap().take().unwrap();
        setter.join().unwrap();
        let mut got = Vec::new();
        while let Ok(ev) = reader.try_recv() {
            got.push(pv.delivered(&ev).value);
        }
        assert_eq!(got.last(), Some(&EpicsValue::Double(1.0)), "{got:?}");
    }

    /// CANCEL: the removed monitor keeps its consumer's stream open until the
    /// caller drops what `remove_subscriber` returned.
    #[test]
    fn a_removed_monitor_ends_its_stream_when_dropped() {
        let pv = pv();
        let mut reader = pv
            .add_subscriber(1, DbFieldType::Double, DBE_VALUE)
            .expect("added");
        let gone = pv.remove_subscriber(reader.id());
        assert_eq!(gone.len(), 1);
        assert_eq!(pv.subscriber_count(), 0);
        assert!(matches!(reader.try_recv(), Err(TryRecvError::Empty)));
        drop(gone);
        assert!(matches!(reader.try_recv(), Err(TryRecvError::Disconnected)));
    }

    /// At the cap an add is refused and publishes nothing; a dropped reader
    /// frees its place.
    #[test]
    fn an_add_at_the_cap_is_refused_until_a_row_closes() {
        // SAFETY: nextest runs each test in its own process.
        // 8 is the floor `max_subscribers_per_pv` applies.
        unsafe { std::env::set_var("EPICS_CAS_MAX_SUBSCRIBERS_PER_PV", "8") };
        let pv = pv();
        let mut held: Vec<_> = (0..8)
            .map(|sid| pv.add_subscriber(sid, DbFieldType::Double, DBE_VALUE))
            .collect();
        assert!(held.iter().all(Result::is_ok));
        assert_eq!(
            pv.add_subscriber(8, DbFieldType::Double, DBE_VALUE).err(),
            Some(SubscribeRefused::CapReached)
        );
        assert_eq!(pv.subscriber_count(), 8, "a refused add published");
        drop(held.pop());
        held.push(pv.add_subscriber(9, DbFieldType::Double, DBE_VALUE));
        assert!(held[7].is_ok());
        assert_eq!(pv.subscriber_count(), 8);
    }

    /// A reader dropped with no CANCEL unlinks its row as it drops: the map a
    /// post walks holds no row whose reader is gone, with no add between.
    #[test]
    fn a_dropped_reader_leaves_the_walked_map_at_once() {
        let pv = pv();
        let kept = pv
            .add_subscriber(1, DbFieldType::Double, DBE_VALUE)
            .expect("added");
        let dropped = pv
            .add_subscriber(2, DbFieldType::Double, DBE_VALUE)
            .expect("added");
        let id = dropped.id();
        drop(dropped);
        assert_eq!(pv.subscriber_count(), 1, "a gone reader's row is walked");
        assert!(pv.subscribers.load().of(0).all(|s| s.id() == kept.id()));
        assert!(pv.remove_subscriber(id).is_empty());
        drop(pv);
        drop(kept);
    }
}
