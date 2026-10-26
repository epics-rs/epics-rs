//! What `dbGet` hands a CA reader for one request: the read chain, the post
//! stage, the long-string boundary conversion and the options `getOptions`
//! takes from the record (`dbAccess.c:336-414`, `dbEvent.c:984-985`). rsrv's
//! READ and event task and the in-process CA link channel all encode
//! through it.

use std::borrow::Cow;

use super::channel_io::{ChannelTarget, LongStringMode, read_ack_state};
use super::db_access::GetFailed;
use crate::server::pv::{LogValue, MonitorEvent};
use crate::server::snapshot::Snapshot;
use crate::types::EpicsValue;

/// A `$` channel's delivery: the value as `dbGet` reads it through the
/// `DBR_CHAR` address, see
/// [`posted_char_view`](crate::server::database::char_view::posted_char_view).
/// `requested_count` is `dbGet`'s `nRequest` (0 for autosize); the wire
/// pads to a fixed request count, as C `read_reply` does.
pub fn apply_long_string(
    v: crate::types::EpicsValue,
    no_elements: u32,
    nul_at_count: bool,
    requested_count: u32,
) -> crate::types::EpicsValue {
    let n = match requested_count {
        0 => no_elements,
        r => r.min(no_elements),
    };
    crate::server::database::char_view::posted_char_view(v, n as usize, nul_at_count)
}

/// Convert a long-string *record* field's snapshot value from
/// `EpicsValue::CharArray` to a scalar `EpicsValue::String`. C
/// `cvt_dbaddr` presents lsi/lso VAL & OVAL and printf VAL as a scalar
/// `DBF_STRING` with `no_elements = 1` (lsiRecord.c:141-143,
/// lsoRecord.c:183-185, printfRecord.c:415-417); the record stores the
/// value as a NUL-terminable CHAR array (the long-string carrier). This
/// is the inverse of [`apply_long_string`] — the conversion the CA
/// boundary applies for *plain* (non-`$`) access so the channel ships a
/// single `DBR_STRING` element. The buffer is decoded verbatim (no
/// UTF-8 validation, matching pvxs raw-byte storage) up to its first
/// NUL, past `LEN` included
/// ([`long_string_field_bytes`](crate::server::database::char_view::long_string_field_bytes));
/// the DBR_STRING encoder then truncates to `MAX_STRING_SIZE`
/// (= 40), so an over-long value clips on the wire exactly as C does.
/// Non-`CharArray` values pass through unchanged.
///
/// The carrier holds `LEN` elements, and `dbGet` reads `min(LEN, 1)`
/// (`get_array_info` against the capacity of 1, `dbAccess.c:936-1001`), so
/// an empty carrier is no element, not `""`.
pub fn apply_native_long_string(v: crate::types::EpicsValue) -> crate::types::EpicsValue {
    use crate::types::EpicsValue;
    match v {
        EpicsValue::CharArray(b) | EpicsValue::UCharArray(b) if b.is_empty() => {
            EpicsValue::StringArray(Vec::new().into())
        }
        v => crate::server::database::char_view::long_string_field_text(v),
    }
}

/// Apply the boundary conversion selected by `mode` to a delivery
/// snapshot before DBR encoding.
pub fn apply_long_string_mode(
    v: crate::types::EpicsValue,
    mode: LongStringMode,
    requested_count: u32,
) -> crate::types::EpicsValue {
    match mode {
        LongStringMode::DollarChar {
            no_elements,
            nul_at_count,
            ..
        } => apply_long_string(v, no_elements, nul_at_count, requested_count),
        LongStringMode::NativeString => apply_native_long_string(v),
        LongStringMode::Plain | LongStringMode::Text(_) => v,
    }
}

/// What one CA subscription does to each event between its queue and the
/// wire: the channel's post chain, on a `$` channel's char image, then the
/// long-string boundary conversion, then for a link or an attribute the text
/// `dbGet` reads for the request type, and for a string field read as
/// `DBR_ENUM` its `cvt_st_e`. Every delivery path reads its events
/// through this one owner, as C's event task runs the post chain for every
/// `event_read` (`dbEvent.c:984-985`).
#[derive(Clone)]
pub struct DeliveryStage {
    /// What an autosize request reads, C `paddr->no_elements`.
    autosize: u32,
    /// `None` when every event goes out exactly as posted, the common case:
    /// the stage is read on every update, so it stays one pointer wide there.
    transform: Option<Box<StageTransform>>,
}

#[derive(Clone)]
struct StageTransform {
    post: Option<std::sync::Arc<crate::server::database::db_access::PostStage>>,
    long_string_mode: LongStringMode,
    text: Option<TextStep>,
    st_e: Option<EnumSource>,
}

/// The record a `DBF_STRING` field's `DBR_ENUM` read converts against:
/// `cvt_st_e` asks the field's own record (`dbFastLinkConv.c:247-281`).
#[derive(Clone)]
struct EnumSource {
    record: std::sync::Arc<crate::server::record::RecordCell>,
    field: std::sync::Arc<str>,
}

impl EnumSource {
    /// The source of a `data_type` request on `target`, read in `mode`;
    /// `None` where no read of it reaches `cvt_st_e`. The value decides at
    /// each read ([`cvt_st_e_input`](crate::server::record::cvt_st_e_input));
    /// the address type only spares every other field the transform.
    fn of(target: &ChannelTarget, mode: LongStringMode, data_type: u16) -> Option<Self> {
        use crate::types::DbFieldType;
        let ChannelTarget::RecordField { record, field } = target else {
            return None;
        };
        if crate::types::native_type_for_dbr(data_type).ok() != Some(DbFieldType::Enum) {
            return None;
        }
        let string = match mode {
            LongStringMode::NativeString => true,
            LongStringMode::Plain => {
                let inst = record.read();
                record
                    .addressed_field_type(field, Some(&*inst.record))
                    .is_none_or(|t| t.served_type() == Some(DbFieldType::String))
            }
            LongStringMode::DollarChar { .. } | LongStringMode::Text(_) => false,
        };
        string.then(|| Self {
            record: record.clone(),
            field: field.clone(),
        })
    }

    /// `value` as `dbGet` reads it for `DBR_ENUM`: the state index
    /// [`cvt_st_e`](crate::server::record::cvt_st_e) answers for one string,
    /// `Err` where it fails, after printing C's line.
    fn read(&self, value: &mut EpicsValue) -> Result<(), GetFailed> {
        let Some(s) = crate::server::record::cvt_st_e_input(value) else {
            return Ok(());
        };
        let index = crate::server::record::cvt_st_e(&*self.record.read().record, s);
        match index {
            Ok(i) => {
                *value = EpicsValue::Enum(i);
                Ok(())
            }
            Err(report) => {
                report.emit(
                    &self.record.name().to_str_lossy(),
                    &self.record.put_name(&self.field).0,
                );
                Err(GetFailed)
            }
        }
    }
}

/// `dbGet`'s read of a link or an attribute for the stage's request type.
#[derive(Clone, Copy)]
enum TextStep {
    Read(crate::server::database::char_view::TextRead),
    /// `S_db_badDbrtype`: every update is `read_reply`'s GETFAIL
    /// (`camessage.c:540-557`).
    Refused,
}

impl DeliveryStage {
    /// The stage for a `data_type` request on `target`, a channel whose post
    /// chain is `post`, on a field of `autosize` elements.
    pub fn new(
        target: &ChannelTarget,
        post: crate::server::database::filters::FilterChain,
        long_string_mode: LongStringMode,
        autosize: u32,
        data_type: u16,
    ) -> Self {
        let dollar = match long_string_mode {
            LongStringMode::DollarChar { view, .. } if !post.is_empty() => Some(view),
            _ => None,
        };
        let route = match long_string_mode {
            LongStringMode::Text(route) => Some(route),
            LongStringMode::DollarChar { view, .. } => view.source().text_route(),
            _ => None,
        };
        // `DBR_CLASS_NAME` reads the record type, not the field
        // (`db_access.c:784-800`).
        let text = route
            .filter(|_| data_type != crate::types::DBR_CLASS_NAME)
            .map(|route| {
                crate::types::native_type_for_dbr(data_type)
                    .ok()
                    .and_then(|target| route.read(target))
                    .map_or(TextStep::Refused, TextStep::Read)
            });
        let post = crate::server::database::db_access::PostStage::new(post, dollar)
            .map(std::sync::Arc::new);
        let st_e = EnumSource::of(target, long_string_mode, data_type);
        let transform = (post.is_some()
            || long_string_mode != LongStringMode::Plain
            || st_e.is_some())
        .then(|| {
            Box::new(StageTransform {
                post,
                long_string_mode,
                text,
                st_e,
            })
        });
        Self {
            autosize,
            transform,
        }
    }

    /// A stage that sends every event as posted.
    pub fn plain() -> Self {
        Self {
            autosize: 0,
            transform: None,
        }
    }

    /// `read_reply`'s `item_count` for a `requested_count` request
    /// (`camessage.c:510-514`).
    pub fn item_count(&self, requested_count: u32) -> u32 {
        match requested_count {
            0 => self.autosize,
            n => n,
        }
    }

    /// Whether every event goes out exactly as posted.
    pub fn is_identity(&self) -> bool {
        self.transform.is_none()
    }

    /// Whether `dbGet` refuses the stage's request type, so that every
    /// update fails.
    pub fn refuses(&self) -> bool {
        self.transform
            .as_ref()
            .is_some_and(|t| matches!(t.text, Some(TextStep::Refused)))
    }

    /// `event` as sent for a `requested_count` request, `None` where the post
    /// chain drops it. The caller has answered [`Self::refuses`].
    fn prepare(
        &self,
        event: crate::server::pv::MonitorEvent,
        requested_count: u32,
    ) -> Option<crate::server::pv::MonitorEvent> {
        let Some(transform) = &self.transform else {
            return Some(event);
        };
        // An autosize request reads the field's own count, which a log the
        // filters grew (`ts`) is cut to.
        let n_request = self.item_count(requested_count) as usize;
        let mut event = Self::convert(transform, event, requested_count, n_request)?;
        if let Some(TextStep::Read(read)) = transform.text {
            let text = event.replace_value(crate::types::EpicsValue::Long(0));
            event.replace_value(read.get(
                crate::server::database::char_view::text_of(&text),
                n_request,
            ));
        }
        Some(event)
    }

    /// [`Self::prepare`] before the text read: the post chain and the
    /// long-string conversion.
    fn convert(
        transform: &StageTransform,
        event: crate::server::pv::MonitorEvent,
        requested_count: u32,
        n_request: usize,
    ) -> Option<crate::server::pv::MonitorEvent> {
        let mut event = match &transform.post {
            // A `$` view's read is already `dbGet`'s chars for this count.
            Some(post) if post.reads_chars() => return post.read_request(event, n_request),
            Some(post) => {
                let mut event = post.read(event)?;
                if event.value().as_value().map_or(0, |v| v.count() as usize) > n_request {
                    let mut value = event.replace_value(crate::types::EpicsValue::Long(0));
                    value.truncate(n_request);
                    event.replace_value(value);
                }
                event
            }
            None => event,
        };
        if !matches!(
            transform.long_string_mode,
            LongStringMode::Plain | LongStringMode::Text(_)
        ) {
            let value = event.replace_value(crate::types::EpicsValue::Long(0));
            event.replace_value(apply_long_string_mode(
                value,
                transform.long_string_mode,
                requested_count,
            ));
        }
        Some(event)
    }

    /// [`Self::prepare`] for a snapshot no queue carried: a read, or a single
    /// event posted under subscriber mask `mask`, rewritten in place, then
    /// converted as `dbGet` converts the request type; `None` where the chain
    /// drops it, which leaves `snapshot` without its value, and `Err` where
    /// `dbGet` fails it. The caller has answered [`Self::refuses`].
    fn prepare_snapshot(
        &self,
        snapshot: &mut crate::server::snapshot::Snapshot,
        requested_count: u32,
        mask: u16,
    ) -> Option<Result<(), GetFailed>> {
        let Some(transform) = &self.transform else {
            return Some(Ok(()));
        };
        let log = crate::server::pv::MonitorEvent::read_log(
            snapshot,
            crate::server::recgbl::EventMask::from_bits(mask),
        );
        let log = self.prepare(log, requested_count)?;
        log.write_log_over(snapshot);
        Some(
            transform
                .st_e
                .as_ref()
                .map_or(Ok(()), |source| source.read(&mut snapshot.value)),
        )
    }
}

/// One queued update as `dbGet` encodes it (`dbAccess.c:909`): its log,
/// and where the encode reads an option, the channel read at delivery with
/// the log written over it, whose status, severity, amsg, time and utag are
/// the log's and every other option the channel's (`getOptions`,
/// `dbAccess.c:336-414`). The read lives in the caller's slot; the update
/// points at it, so neither is moved on the way to the encode.
pub struct Update<'a> {
    /// Borrowed from the consumer's queue slot unless a post chain rewrites it.
    log: Cow<'a, MonitorEvent>,
    body: Body<'a>,
}

/// An [`Update`] after its channel's stage, as the encode reads it.
pub struct Delivery<'a> {
    log: &'a MonitorEvent,
    /// `Err` where the stage [refuses](DeliveryStage::refuses) the request
    /// type.
    body: Result<Body<'a>, GetFailed>,
}

enum Body<'a> {
    /// The log alone, with its scalar made into an `EpicsValue` once for
    /// the count and the encode; `None` when the log's value is boxed.
    Log(Option<EpicsValue>),
    Read(&'a mut Snapshot),
}

/// The scalar a log carries by value.
fn scalar(value: LogValue<'_>) -> Option<EpicsValue> {
    match value {
        LogValue::Val(v) => Some(v),
        _ => None,
    }
}

impl<'a> Update<'a> {
    /// The log alone: the encode reads value, alarm and time only.
    pub fn log(log: Cow<'a, MonitorEvent>) -> Self {
        let body = Body::Log(scalar(log.value()));
        Self { log, body }
    }

    /// An update of `read` written over by its own log.
    pub fn of_read(read: &'a mut Snapshot) -> Self {
        let log =
            MonitorEvent::read_log(&mut read.clone(), crate::server::recgbl::EventMask::VALUE);
        Self {
            log: Cow::Owned(log),
            body: Body::Read(read),
        }
    }

    /// The update after this channel's post chain and long-string
    /// conversion, `None` where the chain drops it. The chain takes the log
    /// by value, so the delivery borrows it from `log` rather than `self`.
    pub fn prepare<'s>(
        self,
        stage: &DeliveryStage,
        requested_count: u32,
        log: &'s mut Option<Cow<'a, MonitorEvent>>,
    ) -> Option<Delivery<'s>> {
        let Self { log: posted, body } = self;
        if stage.refuses() {
            return Some(Delivery {
                log: log.insert(posted),
                body: Err(GetFailed),
            });
        }
        if stage.is_identity() {
            return Some(Delivery {
                log: log.insert(posted),
                body: Ok(body),
            });
        }
        match body {
            // A string read as `DBR_ENUM` is always a read: the encode reads
            // `put_enum_str` ([`reads_options`]), so `cvt_st_e` runs here.
            Body::Read(read) => {
                let body = stage
                    .prepare_snapshot(read, requested_count, posted.mask().bits())?
                    .map(|()| Body::Read(read));
                Some(Delivery {
                    log: log.insert(posted),
                    body,
                })
            }
            Body::Log(_) => {
                let log = log.insert(Cow::Owned(
                    stage.prepare(posted.into_owned(), requested_count)?,
                ));
                let body = Ok(Body::Log(scalar(log.value())));
                Some(Delivery { log, body })
            }
        }
    }
}

impl Delivery<'_> {
    pub fn encode_dbr_into(&self, dst: &mut Vec<u8>, data_type: u16) -> crate::error::CaResult<()> {
        let log = self.log;
        let Ok(body) = &self.body else {
            return Err(crate::error::CaError::GetConvertFailed(
                "dbGet refuses the request type (S_db_badDbrtype)".into(),
            ));
        };
        match body {
            Body::Log(Some(v)) => crate::types::encode_log_dbr_into(
                dst,
                data_type,
                v,
                log.stat(),
                log.sevr(),
                log.time(),
            ),
            Body::Log(None) => log.encode_dbr_into(dst, data_type),
            Body::Read(read) => crate::types::encode_dbr_into(dst, data_type, read),
        }
    }

    /// The element count the encode carries.
    pub fn count(&self) -> u32 {
        match &self.body {
            Ok(Body::Log(Some(v))) => v.count(),
            Ok(Body::Log(None)) => self.log.value().as_value().map_or(0, |v| v.count()),
            Ok(Body::Read(read)) => read.value.count(),
            Err(GetFailed) => 0,
        }
    }

    /// The channel read this update encodes, if it reads one.
    pub fn read(&self) -> Option<&Snapshot> {
        match &self.body {
            Ok(Body::Read(read)) => Some(read),
            _ => None,
        }
    }
}

/// `event` as `dbGet` encodes it for `data_type`. `live` is the channel
/// read; it runs only when the encode consults an option — GR/CTRL, STSACK,
/// CLASS_NAME, or a conversion through precision, enum strings or
/// `put_enum_str` — or the log holds no value, and lands in `read`; `None`
/// from it is a failed get.
pub fn with_live_options<'a>(
    data_type: u16,
    event: Cow<'a, MonitorEvent>,
    read: &'a mut Option<Snapshot>,
    live: impl FnOnce() -> Option<Snapshot>,
) -> Option<Update<'a>> {
    let value = event.value();
    if (value.as_value()).is_some_and(|v| !reads_options(data_type, v)) {
        let body = Body::Log(scalar(value));
        return Some(Update { log: event, body });
    }
    drop(value);
    let read = read.insert(live()?);
    event.write_log_over(read);
    Some(Update {
        log: event,
        body: Body::Read(read),
    })
}

/// Whether `encode_dbr_into` reads anything but value, alarm, time and utag
/// for a `data_type` request of `value`.
fn reads_options(data_type: u16, value: &EpicsValue) -> bool {
    use crate::types::DbFieldType;
    use crate::types::{DBR_CLASS_NAME, DBR_STSACK_STRING};
    if matches!(data_type, 21..=34 | DBR_STSACK_STRING | DBR_CLASS_NAME) {
        return true;
    }
    let is_string = matches!(value, EpicsValue::String(_) | EpicsValue::StringArray(_));
    match crate::types::native_type_for_dbr(data_type) {
        Ok(DbFieldType::String) => !is_string,
        Ok(DbFieldType::Enum) => is_string,
        _ => false,
    }
}

/// The value C's `dbChannel_get` reads off an open channel for a
/// `requested_type` request of `requested_count` elements (0 = autosize):
/// the read chain, the post stage and the options `getOptions` takes from
/// the record (`dbAccess.c:361-362`). rsrv's READ and the in-process CA link
/// channel both read through it. `Err` is a request type `dbGet` refuses.
pub fn prepare_read(
    target: &ChannelTarget,
    chain: crate::server::database::filters::FilterChain,
    long_string_mode: LongStringMode,
    field_element_count: u32,
    requested_type: u16,
    requested_count: u32,
    mut snapshot: crate::server::snapshot::Snapshot,
) -> Result<crate::server::snapshot::Snapshot, GetFailed> {
    let (read_chain, post_chain) = chain.split();
    if !read_chain.is_empty() {
        if let Some(v) = read_chain.apply_to_read_value(snapshot.value.clone()) {
            snapshot.value = v;
        }
    }
    // convert String → CharArray of exactly 40 elements BEFORE the
    // requested-count clamp. C read_reply sizes the payload to
    // dbr_size_n(DBR_CHAR, request_count) after the channel reports
    // no_elements=40; the clamp must see the 40-element array so
    // `caget -# N PV.DESC$` trims to N chars (not the pre-convert
    // count of 1 that EpicsValue::String::count() returns).
    // `NativeString` is the inverse — a long-string record field
    // (printf/lsi/lso) decoded from its CHAR carrier to a scalar
    // string so plain access ships one DBR_STRING (C cvt_dbaddr).
    let stage = DeliveryStage::new(
        target,
        post_chain,
        long_string_mode,
        field_element_count,
        requested_type,
    );
    if stage.refuses() {
        return Err(GetFailed);
    }
    if !stage.is_identity() {
        // The post chain drops nothing (`arr`, `ts`), so `None` is
        // unreachable; it keeps the value as read.
        let mut read = snapshot.clone();
        if let Some(prepared) = stage.prepare_snapshot(&mut read, requested_count, 0) {
            prepared?;
            snapshot = read;
        }
    }
    // Respect client's requested element count (e.g. caget -# 10)
    if requested_count > 0 && requested_count < snapshot.value.count() {
        snapshot.value.truncate(requested_count as usize);
    }

    if requested_type == crate::types::DBR_STSACK_STRING {
        read_ack_state(target, &mut snapshot.alarm);
    }

    // For DBR_CLASS_NAME (38) substitute the record's recordType
    // into the response. SimplePv channels have no record-type
    // identity so they receive an empty string (which matches
    // what the C IOC does for in-process DBR_CLASS_NAME reads
    // against synthetic channels).
    if requested_type == crate::types::DBR_CLASS_NAME {
        if let ChannelTarget::RecordField { record, .. } = target {
            let inst = record.read();
            snapshot.class_name = Some(inst.record.record_type().to_string());
        }
    }
    Ok(snapshot)
}
