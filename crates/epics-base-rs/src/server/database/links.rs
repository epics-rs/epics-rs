use std::collections::HashSet;
use std::sync::Arc;

use crate::error::{DbResult, DbStatus};
use crate::server::record::record_instance::FieldMetaSource;
use crate::server::record::{
    AlarmSeverity, NotifyWaitSet, OutTarget, RecordCell, RecordInstance, ScanType,
};
use crate::types::{DbFieldType, EpicsValue, PvString, RecordName};

use super::filters::ChannelName;
use super::link_set::LinkDbfType;
use super::processing::ProcStack;
use super::record_lock::InFrame;
use super::{PvDatabase, SelmKind, SelmResult, select_link_indices_ex};

/// The `record[.FIELD]` name a link target is addressed by in the local
/// database — the name with the channel filter already removed.
///
/// [`DbLink::pvname`](crate::server::record::DbLink::pvname) is C's verbatim
/// `pv_link.pvname`, so for `src.[2]` it is the whole string;
/// [`DbLink::target`](crate::server::record::DbLink::target) is the one place
/// that splits it, and this is the one place that puts the halves back
/// together for a local lookup. The external name stays `pvname`, which keeps
/// the filter because C hands it to `dbCaAddLink` unchanged.
fn local_pv_name(target: &ChannelName) -> Vec<u8> {
    let mut name = local_name(&target.record, &target.field);
    // A `$` address the record refused keeps its `$`, so the put is refused
    // too rather than storing through the field's own type.
    if target.string_view {
        if target.field == "VAL" {
            name.extend_from_slice(b".VAL");
        }
        name.push(b'$');
    }
    name
}

/// [`local_pv_name`] from the halves, for a caller that holds them apart.
pub(super) fn local_name(record: &[u8], field: &str) -> Vec<u8> {
    let mut name = record.to_vec();
    if field != "VAL" {
        name.push(b'.');
        name.extend_from_slice(field.as_bytes());
    }
    name
}

/// **The one classifier of a process-time read that delivered no value.**
///
/// C `dbGetLink` reports `(status, buffer)`, and the two outcomes a `None`
/// collapses are not the same: a CONSTANT (or unset) link is `dbConstGetValue`
/// (`dbConstLink.c:219-225`) — status 0, `*pnRequest = 0`, buffer untouched —
/// while any other link that produced nothing FAILED and owes the reader a LINK
/// alarm. Every process-time reader ends here ([`PvDatabase::read_link_with_alarm`]
/// for the input-fetch/control-link paths, [`PvDatabase::read_link_value_as_with_alarm`] for
/// the `ReadDbLink` executor), so a constant cannot be no-data on one path and a
/// live value on another.
/// C `dbDbGetValue`'s filter arm, and its NULL-log fallback to the raw
/// field (`dbDbLink.c:206-219`): the value a DB link delivers once its
/// target's `json_suffix` filter chain has run over the raw read.
#[inline]
fn filtered_read(
    target: &crate::server::database::filters::ChannelName,
    v: EpicsValue,
) -> EpicsValue {
    match target.json_suffix.as_deref() {
        Some(suffix) => filter_read(suffix, v),
        None => v,
    }
}

/// [`filtered_read`]'s filter arm, out of the unfiltered read's frame.
fn filter_read(suffix: &str, v: EpicsValue) -> EpicsValue {
    crate::server::database::filters::parse_filter_chain(suffix)
        .apply_to_read_value(v.clone())
        .unwrap_or(v)
}

/// What a one-element request converts of `value`: an array's element 0, as
/// `dbGet` and `dbCaGetLink` convert from offset 0, and of an empty array
/// nothing, a failed read.
#[inline]
pub(super) fn element_0(value: EpicsValue) -> Option<EpicsValue> {
    if value.is_array() {
        value.first_element()
    } else {
        Some(value)
    }
}

/// C `dbGetLink(..., DBR_DOUBLE, ...)` of one element.
#[inline]
pub(super) fn link_value_as_f64(value: EpicsValue) -> Option<f64> {
    element_0(value).and_then(|s| s.get_convert_f64())
}

/// [`link_value_as_f64`] for the integer requests: element 0 through C's GET
/// table, so a string source parses (`cvt_st_l` and siblings) and an integer
/// source narrows by cast.
fn link_value_get_as(value: EpicsValue, target: DbFieldType) -> Option<EpicsValue> {
    element_0(value)?.get_convert(target).ok()
}

fn empty_read_fetch(
    link: &crate::server::record::ParsedLink,
) -> crate::server::recgbl::simm::LinkFetch {
    use crate::server::recgbl::simm::LinkFetch;
    if crate::server::recgbl::simm::has_const_lset(link) {
        LinkFetch::NoData
    } else {
        LinkFetch::Failed
    }
}

/// The string a `DBF_CHAR`/`DBF_UCHAR` source spells, for a reader that asked
/// for [`LinkReadAs::CharArrayAsString`](crate::server::record::LinkReadAs) —
/// C `dbGetLink(&dol, DBF_CHAR, &s, 0, &n_elements)` copying `n` bytes into the
/// reader's char buffer, which every later `strcmp`/`atof` then reads as a C
/// string (`sseqRecord.c:682-696`).
///
/// `max_elements` is the reader's own clamp (C `if (n_elements>40) n_elements=40`
/// against its `char s[40]`). The bytes stop at the first NUL, which is where the
/// C string ends; a scalar `CHAR` source contributes its single byte, exactly as
/// C's `n_elements == 1` read does.
///
/// Any other source is `dbGet`'s `DBR_CHAR` conversion of it (`cvt_l_c`,
/// `cvt_st_c`, ...: one element of a scalar, a string parsed), and a refused
/// conversion is a failed read.
fn char_bytes_as_string(value: &EpicsValue, max_elements: usize) -> Option<PvString> {
    let converted;
    let bytes: &[u8] = match value {
        EpicsValue::CharArray(b) | EpicsValue::UCharArray(b) => b,
        EpicsValue::Char(b) | EpicsValue::UChar(b) => std::slice::from_ref(b),
        other => {
            converted = other.get_convert(DbFieldType::Char).ok()?;
            match &converted {
                EpicsValue::Char(b) => std::slice::from_ref(b),
                EpicsValue::CharArray(b) => b,
                _ => return None,
            }
        }
    };
    let n = bytes.len().min(max_elements);
    let text = &bytes[..n];
    let end = text.iter().position(|&b| b == 0).unwrap_or(n);
    Some(PvString::from_bytes(text[..end].to_vec()))
}

/// C `dbGet` of a `$` address on a link or an attribute: `getLinkValue` /
/// `getAttrValue` answer `read_as` from the text (`dbAccess.c:944-952`), so
/// `image`, its `DBR_CHAR` read, is never what the request converts. `None`
/// is `S_db_badDbrtype`.
fn text_route_read(
    route: super::char_view::TextRoute,
    read_as: crate::server::record::LinkReadAs,
    image: EpicsValue,
) -> Option<EpicsValue> {
    use crate::server::record::LinkReadAs;
    let text = super::char_view::text_of(&image);
    match read_as {
        LinkReadAs::Native => Some(image),
        LinkReadAs::CharArrayAsString { max_elements } => char_bytes_as_string(
            &route.read(DbFieldType::Char)?.get(text, max_elements),
            max_elements,
        )
        .map(EpicsValue::String),
        LinkReadAs::Array { dbr, n_request } => Some(route.read(dbr)?.get(text, n_request)),
        scalar => element_0(route.read(scalar.dbr_type()?)?.get(text, 1)),
    }
}

/// C `dbDBRoldToDBFnew[pca->dbrType]` (`dbCa.c:672`): the link set's cached
/// remote type expressed as the DBF type a device support switches on.
fn link_dbf_to_field_type(t: LinkDbfType) -> DbFieldType {
    match t {
        LinkDbfType::Char => DbFieldType::Char,
        LinkDbfType::UChar => DbFieldType::UChar,
        LinkDbfType::Short => DbFieldType::Short,
        LinkDbfType::UShort => DbFieldType::UShort,
        LinkDbfType::Long => DbFieldType::Long,
        LinkDbfType::ULong => DbFieldType::ULong,
        LinkDbfType::Int64 => DbFieldType::Int64,
        LinkDbfType::UInt64 => DbFieldType::UInt64,
        LinkDbfType::Float => DbFieldType::Float,
        LinkDbfType::Double => DbFieldType::Double,
        LinkDbfType::String => DbFieldType::String,
        LinkDbfType::Enum => DbFieldType::Enum,
    }
}

/// Inverse of [`link_dbf_to_field_type`]: a local DB link's target field type
/// expressed as the link-set DBF code. C `dbDbGetDBFtype` reports the target's
/// `dbChannelFinalFieldType` verbatim (`dbDbLink.c:151-155`), so this is a
/// straight relabelling, not a conversion.
fn field_type_to_link_dbf(t: DbFieldType) -> LinkDbfType {
    match t {
        DbFieldType::Char => LinkDbfType::Char,
        DbFieldType::UChar => LinkDbfType::UChar,
        DbFieldType::Short => LinkDbfType::Short,
        DbFieldType::UShort => LinkDbfType::UShort,
        DbFieldType::Long => LinkDbfType::Long,
        DbFieldType::ULong => LinkDbfType::ULong,
        DbFieldType::Int64 => LinkDbfType::Int64,
        DbFieldType::UInt64 => LinkDbfType::UInt64,
        DbFieldType::Float => LinkDbfType::Float,
        DbFieldType::Double => LinkDbfType::Double,
        DbFieldType::String => LinkDbfType::String,
        DbFieldType::Enum => LinkDbfType::Enum,
    }
}

/// dfanout output-link fields, index-aligned with `MultiOut::Dfanout`
/// (`dfanoutRecord.c:39` — OUTA..OUTP). Named so a failed OUTn put can
/// report WHICH link failed in the record's AMSG, as C `setLinkAlarm` does.
pub(crate) const DFANOUT_LINK_FIELDS: [&str; 16] = [
    "OUTA", "OUTB", "OUTC", "OUTD", "OUTE", "OUTF", "OUTG", "OUTH", "OUTI", "OUTJ", "OUTK", "OUTL",
    "OUTM", "OUTN", "OUTO", "OUTP",
];

/// seq / fanout link fields, index-aligned with `MultiOut::Seq` /
/// `MultiOut::Fanout` (`seqRecord.c:86`, `fanoutRecord.c:39` — LNK0..LNKF).
pub(crate) const LNK_LINK_FIELDS: [&str; 16] = [
    "LNK0", "LNK1", "LNK2", "LNK3", "LNK4", "LNK5", "LNK6", "LNK7", "LNK8", "LNK9", "LNKA", "LNKB",
    "LNKC", "LNKD", "LNKE", "LNKF",
];

const SELL: &[&str] = &["SELL"];

/// Every link a cycle reads through [`RecordInstance::record_link`], by
/// record type. A field with no slot would read as no link, so the load
/// refuses it ([`check_record_links`]) and the cycle keeps no check.
const RECORD_LINK_USES: &[(&str, &[&[&str]])] = &[
    ("fanout", &[&LNK_LINK_FIELDS, SELL]),
    ("dfanout", &[&DFANOUT_LINK_FIELDS, SELL]),
    ("seq", &[&DOL_LINK_FIELDS, &LNK_LINK_FIELDS, SELL]),
];

/// Refuse a record whose type reads a field through `record_link` that is
/// not one of its slotted links, naming the field.
pub(super) fn check_record_links(instance: &RecordInstance) -> crate::error::CaResult<()> {
    check_record_links_in(instance, RECORD_LINK_USES)
}

fn check_record_links_in(
    instance: &RecordInstance,
    uses: &[(&str, &[&[&str]])],
) -> crate::error::CaResult<()> {
    let rtype = instance.record.record_type();
    let fields = uses
        .iter()
        .filter(|(t, _)| *t == rtype)
        .flat_map(|(_, f)| f.iter().copied().flatten());
    for field in fields {
        if !instance.has_link_slot(field) {
            return Err(crate::error::CaError::DbLoadFailed(format!(
                "{}: {rtype}.{field} is read as a link but is not a link the type declares",
                instance.name
            )));
        }
    }
    Ok(())
}

/// seq desired-output link fields, index-aligned with `MultiOut::Seq`
/// (`seqRecord.c:86` — DOL0..DOLF). The names C's `dbLinkFieldName` yields
/// for the `setLinkAlarm` AMSG of a failed group read.
pub(crate) const DOL_LINK_FIELDS: [&str; 16] = [
    "DOL0", "DOL1", "DOL2", "DOL3", "DOL4", "DOL5", "DOL6", "DOL7", "DOL8", "DOL9", "DOLA", "DOLB",
    "DOLC", "DOLD", "DOLE", "DOLF",
];

/// seq DOn value-storage fields (`linkGrp.dov`), index-aligned with DOLn.
const SEQ_DO_FIELDS: [&str; 16] = [
    "DO0", "DO1", "DO2", "DO3", "DO4", "DO5", "DO6", "DO7", "DO8", "DO9", "DOA", "DOB", "DOC",
    "DOD", "DOE", "DOF",
];

/// Alarm state from a link source, used for MS/NMS propagation.
///
/// `amsg` is the alarm-message string — propagated from the source
/// record's `common.amsg` so a downstream MS link sees the same
/// human-readable explanation. Empty when the source has no message
/// or when the link source is not a DB record.
#[derive(Clone, Debug)]
pub(crate) struct LinkAlarm {
    pub stat: u16,
    pub sevr: AlarmSeverity,
    pub amsg: crate::server::record::SparseText,
}

/// A link read's source alarm together with the record it came off.
///
/// C's inheritance tail holds `dbChannelRecord(chan)` and its self-exclusion
/// guard is a POINTER comparison — `if (!status && precord !=
/// dbChannelRecord(chan))` (`dbDbLink.c:228`). Carrying the handle beside the
/// alarm is what lets the port ask that same question: the reader's cell and
/// the source's cell are both already in hand at the read, so
/// [`PvDatabase::input_link_inheritance`] compares them with `Arc::ptr_eq`
/// instead of resolving both names through the alias table and comparing the
/// strings — two map reads and two `String` allocations per link per cycle for
/// a question the pointers answer. Alias-awareness (epics-base PR #336) then
/// holds by construction: an alias and its canonical name reach one cell.
#[derive(Clone)]
pub(crate) struct SourceAlarm {
    pub(crate) alarm: LinkAlarm,
    /// The LOCAL record the alarm was read from. `None` when the source has no
    /// record in this IOC — an external channel, a `lnkCalc`, a `lnkState` —
    /// where C's guard compares `precord` against nothing and cannot fire.
    pub(crate) record: Option<SourceId>,
}

/// The identity of a local source record — the `dbChannelRecord(chan)`
/// operand of C's `precord != dbChannelRecord(chan)` guard, and nothing
/// more: an address, not a handle. The comparison is always against the
/// READER, which is alive for the whole read, so no other cell can be at
/// its address; a source that has since gone away compares unequal, as it
/// should.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct SourceId(usize);

impl SourceId {
    pub(crate) fn of(rec: &RecordCell) -> Self {
        SourceId(std::ptr::from_ref(rec) as usize)
    }

    /// The frame's own record. Identity only: nothing is read through it.
    pub(crate) fn of_frame(frame: InFrame<'_>) -> Self {
        Self::of(frame.unframed())
    }
}

impl std::fmt::Debug for SourceAlarm {
    /// `RecordCell` is not `Debug` (printing one would take its lock), so the
    /// handle shows as the identity question it exists to answer.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SourceAlarm")
            .field("alarm", &self.alarm)
            .field("local_record", &self.record.is_some())
            .finish()
    }
}

impl SourceAlarm {
    /// The committed alarm of a LOCAL source record, read off the cell the
    /// value was just read from.
    /// `alarm` is the source's committed alarm as read under the SAME lock
    /// hold that produced the value ([`PvDatabase::read_db_target_by_name`]), so the
    /// pair is one snapshot of the source and costs the target one lock.
    pub(crate) fn local(record: SourceId, alarm: LinkAlarm) -> Self {
        Self {
            alarm,
            record: Some(record),
        }
    }

    /// A source with no local record: the alarm an external lset reported.
    pub(crate) const fn external(alarm: LinkAlarm) -> Self {
        Self {
            alarm,
            record: None,
        }
    }
}

impl LinkAlarm {
    /// The record's PENDING alarm (`nsta`/`nsev`/`namsg`) — what an OUT-link
    /// put inherits into its target. C `dbDbPutValue` calls
    /// `recGblInheritSevrMsg(..., pdest, psrce->nsta, psrce->nsev,
    /// psrce->namsg)` (dbDbLink.c:382-383): `dbPutLink` runs from inside the
    /// source's `process()`, BEFORE `recGblResetAlarms` commits the cycle, so
    /// the alarm the source raised THIS cycle is still only pending. Reading
    /// the committed `stat`/`sevr` here would carry the PREVIOUS cycle's
    /// severity to every MS-class target.
    ///
    /// Read at each put, after the put itself, as C does: an earlier link's
    /// failure in the same cycle has already raised the source's LINK_ALARM.
    pub(crate) fn pending(common: &crate::server::record::CommonFields) -> Self {
        LinkAlarm {
            stat: common.nsta,
            sevr: common.nsev,
            amsg: common.namsg.clone(),
        }
    }

    /// The record's COMMITTED alarm (`stat`/`sevr`/`amsg`) — what an INPUT
    /// link read inherits from the record it read. C `dbDbGetValue` calls
    /// `recGblInheritSevrMsg(..., dbChannelRecord(chan)->stat, ->sevr,
    /// ->amsg)` (dbDbLink.c:229-232): the source there is a foreign record
    /// that finished its own cycle, so its alarm is the committed one.
    pub(crate) fn committed(common: &crate::server::record::CommonFields) -> Self {
        LinkAlarm {
            stat: common.stat,
            sevr: common.sevr,
            amsg: common.amsg.clone(),
        }
    }
}

/// Apply C `recGblInheritSevrMsg` (recGbl.c:263-281) for one MS-class
/// link: fold the link source's alarm (`src`) into the destination
/// record's PENDING alarm (`dest`) per the maximize-severity mode `ms`.
///
/// * **NMS** — no propagation.
/// * **MS**  — raise dest severity to `src.sevr` under `LINK_ALARM`
///   (NOT the source's stat); no message.
/// * **MSI** — same as MS, but only when the source is at `INVALID`.
/// * **MSS** — copy the source's stat + severity + amsg (the only mode
///   that propagates the message).
///
/// Shared by the INPUT-link read path (`processing.rs`, where `dest` is
/// the record reading its inputs) and the DB OUT-link write path
/// (`write_db_link_value_in`, C `dbDbPutValue` →
/// `recGblInheritSevrMsg`, dbDbLink.c:382-383, where `dest` is the
/// OUT-link target). One implementation keeps the two sides from
/// diverging — an earlier INPUT-side variant wrongly treated MS like
/// MSS (propagating source stat + amsg through plain MS).
pub(crate) fn inherit_sevr_msg(
    dest: &mut crate::server::record::CommonFields,
    ms: crate::server::record::MonitorSwitch,
    src: &LinkAlarm,
) {
    use crate::server::recgbl::{rec_gbl_set_sevr, rec_gbl_set_sevr_msg};
    match inherited(ms, src) {
        Some((stat, sevr, Some(msg))) => {
            rec_gbl_set_sevr_msg(dest, stat, sevr, msg);
        }
        Some((stat, sevr, None)) => {
            rec_gbl_set_sevr(dest, stat, sevr);
        }
        None => {}
    }
}

/// C `recGblSetSevrMsg` into a pending alarm: only a strictly higher
/// severity replaces it, so a run of raises leaves the first of the highest.
fn raise(pending: &mut Option<LinkAlarm>, alarm: LinkAlarm) {
    let held = pending
        .as_ref()
        .map_or(crate::server::record::AlarmSeverity::NoAlarm, |p| p.sevr);
    if alarm.sevr as u16 > held as u16 {
        *pending = Some(alarm);
    }
}

/// C `dbLinkFieldName` of a link that is no field of its record
/// (`dbLink.c:66`): a lnkCalc input or `out` child.
const CHILD_LINK_FIELD: &str = "????";

/// `lnkCalc.c:578-582` and `:651-656`: a reader whose time comes from its
/// device takes the stamp the link holds for its `time` input.
#[cold]
fn stamp_device_time(reader: &RecordCell, (time, utag): (crate::types::WallTime, u64)) {
    let mut inst = reader.write();
    if inst.common.stamp_mode() == crate::server::record::StampMode::DeviceTime {
        inst.common.time = time;
        inst.common.utag = utag;
    }
}

/// [`LinkSet::read_time_stamp`](super::LinkSet::read_time_stamp): the lset's
/// `getValue` writes `plink->precord->time` and `utag` whatever the record's
/// `TSE`.
#[cold]
fn stamp_reader(reader: &RecordCell, (time, utag): (crate::types::WallTime, u64)) {
    let mut inst = reader.write();
    inst.common.time = time;
    inst.common.utag = utag;
}

/// The status, severity and (MSS alone) message [`inherit_sevr_msg`] raises
/// on the reader; `None` when `ms` passes nothing on.
#[inline(always)]
fn inherited(
    ms: crate::server::record::MonitorSwitch,
    src: &LinkAlarm,
) -> Option<(
    u16,
    crate::server::record::AlarmSeverity,
    Option<&crate::server::record::SparseText>,
)> {
    use crate::server::recgbl::alarm_status::LINK_ALARM;
    use crate::server::record::{AlarmSeverity, MonitorSwitch};
    match ms {
        MonitorSwitch::Maximize => Some((LINK_ALARM, src.sevr, None)),
        MonitorSwitch::MaximizeIfInvalid => {
            (src.sevr == AlarmSeverity::Invalid).then_some((LINK_ALARM, src.sevr, None))
        }
        MonitorSwitch::MaximizeStatus => Some((src.stat, src.sevr, Some(&src.amsg))),
        MonitorSwitch::NoMaximize => None,
    }
}

/// The source record's per-cycle state propagated to a DB OUT-link
/// target, captured once from the source and threaded to every OUT-link
/// write so all targets in one cycle see the same snapshot:
///
/// * `putf` / `notify` — the PUTF bit and put-notify wait-set that C
///   `processTarget` carries to each target (dbDbLink.c:460-474).
/// * `field` — the source's link FIELD name (`OUT`, `SIOL`, `OUTA`, …),
///   which C `setLinkAlarm` puts in the source's alarm message on a failed
///   put (`dbLink.c:319-323`: `"field %s", dbLinkFieldName(plink)`).
///
/// Bundled so the OUT-link write path threads one snapshot instead of
/// four positional arguments. The FLNK process-trigger path keeps the
/// lighter `PutNotifyCtx` (a forward link propagates no value and thus
/// no alarm).
#[derive(Clone, Copy)]
pub(crate) struct OutLinkSrc<'a> {
    pub putf: bool,
    pub notify: OutNotify<'a>,
    pub field: &'static str,
}

/// The put-notify wait-set an OUT write carries.
#[derive(Clone, Copy)]
pub(crate) enum OutNotify<'a> {
    None,
    /// The chain the source record is in. A DB target joins it (dbNotify);
    /// an external link's put stays plain: C `dbPutLink` is `dbCaPutLink`
    /// (`dbCa.c:627-631`), and pvxs `pvaPutValue` does not wait.
    Chain(&'a Arc<NotifyWaitSet>),
    /// C `dbCaPutLinkCallback` (`sseqRecord.c:720`): the writer asked for the
    /// external put's completion, which reports to the set too.
    Callback(&'a Arc<NotifyWaitSet>),
}

impl<'a> OutNotify<'a> {
    pub(crate) fn chain(set: Option<&'a Arc<NotifyWaitSet>>) -> Self {
        set.map_or(Self::None, Self::Chain)
    }

    /// The set a DB target joins.
    pub(crate) fn wait_set(self) -> Option<&'a Arc<NotifyWaitSet>> {
        match self {
            Self::None => None,
            Self::Chain(set) | Self::Callback(set) => Some(set),
        }
    }
}

/// What [`PvDatabase::put_out_async`] did with the write.
pub(crate) enum AsyncPutStatus {
    /// `putAsync` took it: the record is PACT until the put completes.
    Taken,
    /// Nothing more to do this cycle: no value to put.
    Done,
    /// `putAsync` refused the put, alarmed; `dbPutLinkAsync` returns its
    /// status (`dbLink.c:468-472`), which `write_ao` hands to `process()`.
    Refused,
    /// `S_db_noLSET`: the write goes through `dbPutLink` as a plain one.
    NoLset,
}

/// Who an external put reports its completion to.
#[derive(Clone, Copy)]
pub(crate) enum ExternalDone<'a> {
    /// C `dbCaPutLink`.
    None,
    /// `dbCaPutLinkCallback`, reporting to the writer's wait-set.
    Notify(&'a Arc<NotifyWaitSet>),
    /// `dbCaPutAsync`, whose callback completes the record.
    Record(&'a super::DeviceDone),
}

/// C `processTarget`'s `psrc`: the source record, whose PACT reads 1 while
/// the target processes (`dbDbLink.c:457`, `:514`), and the PUTF and
/// wait-set the target takes from it.
#[derive(Clone, Copy)]
pub(crate) struct TargetSrc<'a> {
    pub rec: Option<&'a Arc<RecordCell>>,
    pub putf: bool,
    pub notify: Option<&'a Arc<NotifyWaitSet>>,
}

impl OutLinkSrc<'_> {
    /// The completion this put's external staging reports to.
    fn external_done(&self) -> ExternalDone<'_> {
        match self.notify {
            OutNotify::Callback(notify) => ExternalDone::Notify(notify),
            OutNotify::None | OutNotify::Chain(_) => ExternalDone::None,
        }
    }

    /// The link this put leaves through, which keys its external staging.
    /// Built only on the external arm, so a DB write pays nothing for it.
    fn link_id(&self, src_rec: Option<InFrame<'_>>) -> super::LinkId {
        match src_rec {
            Some(f) => super::LinkId {
                rec: f.unframed().index(),
                field: self.field,
                install: super::LinkInstall::of_field(&f.read(), self.field),
            },
            None => super::LinkId {
                rec: None,
                field: self.field,
                install: super::LinkInstall::Detached,
            },
        }
    }
}

/// A frame's OUT-link write, captured from the link's cache before the frame
/// releases its guard: the lent target when the cache holds one at the
/// current name revision, else the link. [`Self::capture`] is the only way
/// to a lent target, so the frame never dereferences a stale one.
pub(crate) struct OutLinkWrite(OutWrite);

enum OutWrite {
    Lent(LentOut),
    Link(Arc<crate::server::record::ParsedLink>),
}

/// C `dbDbPutValue`'s `dbAddr`, with what the parse settled about it.
struct LentOut {
    target: crate::server::record::record_instance::OutTarget,
    field: &'static crate::server::record::FieldDesc,
    acts: TargetActs,
}

/// The `processTarget` a lent OUT put owes once it has succeeded, handed
/// out of the writing frame by [`PvDatabase::write_out_hop`].
pub(crate) struct OutHop {
    pub(super) target: crate::server::record::record_instance::OutTarget,
    pub(super) gate: ProcessTargetGate,
}

// SAFETY: an `OutHop` leaves the frame only into its parked cycle on the
// chain's `ProcStack`, which the chain's driver empties before the call
// returns, so the stack moves between threads without one; the target is
// lent only under the owning set, which no other thread holds meanwhile.
unsafe impl Send for OutHop {}

/// What a DB link's parse settled about acting on its local target after
/// the put: C `dbDbPutValue`'s `processTarget` gate and the MS inherit.
#[derive(Clone, Copy)]
struct TargetActs {
    gate: Option<ProcessTargetGate>,
    switch: crate::server::record::MonitorSwitch,
}

impl OutLinkWrite {
    /// The write of the cycle's OUT `value`, if it has one. In the frame
    /// holding `held`, the set of the record whose OUT `cache` is, under
    /// that record's write guard: the only place the target is set, since
    /// after `guard.release()` the cache could be reached only by taking
    /// the guard again.
    #[inline(always)]
    pub(crate) fn capture<V>(
        value: Option<V>,
        cache: &mut crate::server::record::record_instance::OutLinkCache,
        db: &PvDatabase,
        held: &super::SetGuard,
    ) -> Option<(Self, V)> {
        let value = value?;
        Some((Self::of(cache, db, held), value))
    }

    #[inline(always)]
    fn of(
        cache: &mut crate::server::record::record_instance::OutLinkCache,
        db: &PvDatabase,
        held: &super::SetGuard,
    ) -> Self {
        use crate::server::record::record_instance::LinkTargetResolver;
        let target = match cache.out_target() {
            Some(target) if target.revision() == db.name_revision() => target,
            _ => match Self::refresh(cache, db, held) {
                Some(target) => target,
                None => return Self(OutWrite::Link(Arc::clone(cache.shared()))),
            },
        };
        match target.put().field().desc {
            Some(field) => Self(OutWrite::Lent(LentOut {
                target,
                field,
                acts: TargetActs {
                    gate: cache.process_gate(),
                    switch: cache.monitor_switch(),
                },
            })),
            None => Self(OutWrite::Link(Arc::clone(cache.shared()))),
        }
    }

    /// The cache's target at the current name revision, lent as the put
    /// would lend it (`DbLink::lend_out_addr`), or none for a link with no
    /// settled local answer; a stale target never survives it.
    #[cold]
    #[inline(never)]
    fn refresh(
        cache: &mut crate::server::record::record_instance::OutLinkCache,
        db: &PvDatabase,
        held: &super::SetGuard,
    ) -> Option<crate::server::record::record_instance::OutTarget> {
        use crate::server::record::record_instance::{LinkTargetResolver, OutTarget};
        let revision = db.name_revision();
        let target = match &**cache.shared() {
            crate::server::record::ParsedLink::Db(link) => link
                .lend_out_addr(held, revision, || db.resolve_db_link(link))
                .map(|at| OutTarget::new(&at.rec, at.put, revision)),
            _ => None,
        };
        cache.set_out_target(target);
        target
    }
}

/// What [`PvDatabase::write_db_link_value_in`] leaves once the link's
/// address is gone: everything owned, so nothing borrowed from the link or
/// the lend outlives the put.
pub(crate) struct DbLinkWrite {
    /// The local target, kept only when the special's link writes, an
    /// inherit or `processTarget` still act on it.
    target: Option<Arc<crate::server::record::RecordCell>>,
    switch: crate::server::record::MonitorSwitch,
    gate: Option<ProcessTargetGate>,
    failed: bool,
}

impl DbLinkWrite {
    /// A write that reached no local record: the external route's.
    fn untargeted(failed: bool) -> Self {
        Self {
            target: None,
            switch: crate::server::record::MonitorSwitch::NoMaximize,
            gate: None,
            failed,
        }
    }
}

/// The special's link writes and the rest of its `dbPut`, with the put's
/// report owed after them. A caller-owned slot that only a special with
/// link writes fills: returned by value, its ~440 bytes were copied on
/// every write.
pub(crate) struct LinkStage {
    stage: (
        super::field_io::LinkActions,
        super::field_io::LinkTail<'static>,
    ),
    report: Option<(super::field_io::PutTail, std::borrow::Cow<'static, str>)>,
}

/// The [`LinkStage`] slot a link write's caller owns. Empty on every write
/// but a special's, so its drop is an inlined test: `Option<LinkStage>`'s own
/// drop glue went out of line and cost a call per write.
#[derive(Default)]
struct StageSlot(std::mem::ManuallyDrop<Option<LinkStage>>);

impl Drop for StageSlot {
    #[inline(always)]
    fn drop(&mut self) {
        if self.0.is_some() {
            empty_stage_slot(&mut self.0);
        }
    }
}

#[cold]
#[inline(never)]
fn empty_stage_slot(slot: &mut Option<LinkStage>) {
    *slot = None;
}

/// What a DB link's `dbPut` leaves its caller to finish: the special's
/// stage, and the chain that put and its link writes run on.
pub(crate) struct LinkWriteOut<'a> {
    staged: &'a mut Option<LinkStage>,
    visited: &'a mut ProcStack,
}

/// Which of C's two gates a `processTarget` call is coming through — the two
/// are NOT the same test (R18-94).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum ProcessTargetGate {
    /// C `dbScanPassive` (dbDbLink.c:427-434): `if (pto->scan != 0) return 0;`
    /// A Passive target only. This is FLNK (`dbDbScanFwdLink`), fanout `LNKn`,
    /// and the `pvlOptPP` arm of `dbDbPutValue` (:388, `pdest->scan == 0`).
    ScanPassive,
    /// C `dbDbPutValue`'s FIRST arm (dbDbLink.c:387): `dbChannelField(chan) ==
    /// &pdest->proc` — the link addresses the target's `.PROC` field. It
    /// carries NO scan test, so a DB link writing `TARGET.PROC` processes the
    /// target on ANY scan, and is independent of the `PP` flag.
    ///
    /// softIoc: `SRCO.OUT="TGT.PROC"` with `TGT` on `SCAN="10 second"` —
    /// each `dbpf SRCO.PROC 1` advances `TGT.VAL` immediately (0 → 1 → 2).
    /// The port's CA put route already honours this (field_io.rs `.PROC`
    /// handling); this makes the DB-link route agree with it.
    ProcField,
}

impl ProcessTargetGate {
    /// C `dbDbPutValue` (`dbDbLink.c:387-390`) processes the target when the
    /// destination field is `.PROC` **or** the link carries `pvlOptPP` and
    /// the target is Passive: two gates, not one — the `.PROC` arm is
    /// independent of both the PP flag and the target's SCAN (R18-94).
    pub(crate) fn of_put(link: &crate::server::record::DbLink) -> Option<Self> {
        if link.target().field == "PROC" {
            Some(Self::ProcField)
        } else if link.policy == crate::server::record::LinkProcessPolicy::ProcessPassive {
            Some(Self::ScanPassive)
        } else {
            None
        }
    }

    /// Does a target with this SCAN reach `processTarget` through this gate?
    fn admits(self, target_scan: ScanType) -> bool {
        match self {
            Self::ScanPassive => target_scan == ScanType::Passive,
            Self::ProcField => true,
        }
    }
}

/// One selected `seq` link group — C `linkGrp { dly, dol, dov, lnk }`.
#[derive(Clone, Debug)]
pub(crate) struct SeqGroup {
    /// The group's index, 0..F.
    pub idx: usize,
    /// DOLn's parse, from the record's link cache.
    pub dol: Arc<crate::server::record::ParsedLink>,
    /// LNKn's parse, from the record's link cache.
    pub lnk: Arc<crate::server::record::ParsedLink>,
    /// DLYn per-group delay in seconds.
    pub dly: f64,
    /// DOn value-storage field (`linkGrp.dov`) — used when DOLn is
    /// an empty/constant link.
    pub dov: f64,
}

/// Typed multi-output payload — replaces the legacy `\0`-packed
/// `Vec<String>` so a link string containing an embedded NUL can
/// never mis-split (parity review 04-L3).
///
/// `sseq` is NOT a variant here: a `sseq` record drives its per-step
/// `LNKn` writes itself, in `SseqRecord::process()`, through the async
/// PACT machine (C `sseqRecord.c::processCallback`) — not via this
/// all-at-once dispatch.
pub(crate) enum MultiOut {
    /// fanout — each selected forward link (LNK0..LNKF) is read where it is
    /// dispatched, as `fanoutRecord.c` hands `&prec->lnk0 + i` to
    /// `dbScanFwdLink`.
    Fanout,
    /// dfanout — the parse of each selected output link (OUTA..OUTP).
    Dfanout(SelectedLinks),
    /// seq — the selected link groups, in selection order.
    Seq(Vec<SeqGroup>),
}

/// The cached parse of each link a SELM selection picked, by link index; the
/// rest stay `None`, so the cycle reads no link it does not drive.
pub(crate) type SelectedLinks = [Option<Arc<crate::server::record::ParsedLink>>; 16];

impl MultiOut {
    /// Number of link slots — the `count` passed to the SELM selector.
    fn len(&self) -> usize {
        match self {
            MultiOut::Fanout => LNK_LINK_FIELDS.len(),
            MultiOut::Dfanout(v) => v.len(),
            MultiOut::Seq(_) => LNK_LINK_FIELDS.len(),
        }
    }
}

/// The parse of each output-row link that `indices` selects
/// ([`RecordInstance::out_row_link`]).
fn selected_links(instance: &RecordInstance, sel: SelmResult) -> SelectedLinks {
    let mut links: SelectedLinks = Default::default();
    for i in sel.indices() {
        links[i] = Some(instance.out_row_link(i).clone());
    }
    links
}

/// `rec`'s forward links, by field.
fn forward_links(
    rec: &crate::server::record::RecordCell,
) -> Vec<(&'static str, Arc<crate::server::record::ParsedLink>)> {
    let inst = rec.read();
    crate::server::record::declared_fields(inst.record.record_type())
        .filter(|desc| {
            crate::types::link_class_of(desc) == Some(crate::types::DbfLinkClass::FwdLink)
        })
        .filter_map(|desc| Some((desc.name, inst.stored_link_shared(desc.name)?.clone())))
        .collect()
}

/// `dbInitLink`'s warning for a CA forward link that will never forward
/// (`dbLink.c:137-142`); the runtime `dbAddLink` sets the flag silently.
fn warn_ca_forward_without_proc(name: &[u8], field: &str, ca: &crate::server::record::CaLink) {
    if !ca.fwd {
        let name = String::from_utf8_lossy(name);
        crate::runtime::log::errlog_vprintf(format_args!(
            "{}: Forward-link uses Channel Access without pointing to PROC field\n    {name}.{field} => {}\n",
            crate::runtime::log::erl_warning(),
            ca.pv
        ));
    }
}

/// The sixteen-link rows [`PvDatabase::dispatch_multi_output`] reads by
/// index, by record type: the output row, and seq's `DOLn` row.
pub(crate) fn multi_out_rows(
    record_type: &str,
) -> (
    Option<&'static [&'static str; 16]>,
    Option<&'static [&'static str; 16]>,
) {
    match record_type {
        "fanout" => (Some(&LNK_LINK_FIELDS), None),
        "dfanout" => (Some(&DFANOUT_LINK_FIELDS), None),
        "seq" => (Some(&LNK_LINK_FIELDS), Some(&DOL_LINK_FIELDS)),
        _ => (None, None),
    }
}

/// Record types whose multi-output link groups are dispatched by
/// [`PvDatabase::dispatch_multi_output`].
///
/// SINGLE-OWNER INVARIANT — each of these record types' output links
/// (fanout `LNKn`, dfanout `OUTn`, seq `LNKn`) is dispatched (value
/// written + target forward-link processed) **exactly once per process
/// cycle, by `dispatch_multi_output` and by nothing else**.
///
/// `dispatch_multi_output` is the sole owner because it is the only
/// path that performs the full C-record model: SELL→SELN resolution,
/// SELM/OFFS/SHFT selection, per-group DOLn input fetch, and per-group
/// DLYn delay.
///
/// `sseq` is deliberately NOT listed: its `LNKn` writes are owned by
/// `SseqRecord::process()` (the async PACT machine, C
/// `sseqRecord.c::processCallback`), not by this dispatch. `sseq` also
/// does not implement `Record::multi_output_links`, so the generic
/// block skips it for that reason — there is no second dispatcher to
/// gate against.
///
/// MUST NOT: the generic `multi_output_links` block in `processing.rs`
/// (run unconditionally for every record after `dispatch_multi_output`)
/// must skip any record type listed here. `multi_output_dispatch_owned`
/// is consulted by that block (see `run_forward_link_tail_with_putf`
/// §4.6) so a double-dispatch is structurally impossible, not merely
/// removed at one call site.
pub(crate) fn multi_output_dispatch_owned(record_type: &str) -> bool {
    matches!(record_type, "fanout" | "dfanout" | "seq")
}

/// Does a record of this type read `SELL` into `SELN` at any phase?
///
/// The process plan's gate for [`PvDatabase::read_sell_into_seln`], so a
/// cycle can skip the call and the record lock it takes; the function's own
/// match picks the phase.
pub(crate) fn reads_sell(record_type: &str) -> bool {
    matches!(record_type, "dfanout" | "fanout" | "seq")
}

/// Does a record of this type post a named software event from the
/// forward-link tail? The outer gate of [`PvDatabase::dispatch_event_record`].
pub(crate) fn posts_software_event(record_type: &str) -> bool {
    record_type == "event"
}

/// Does a record of this type re-read its subroutine name from a link
/// mid-cycle? The outer gate of `resolve_asub_dynamic_subroutine`.
pub(crate) fn resolves_subroutine_from_link(record_type: &str) -> bool {
    record_type == "aSub"
}

/// Where in the cycle a caller of [`PvDatabase::read_sell_into_seln`] sits.
/// Each SELL-owning record answers to exactly one of these — see that
/// function for the per-record C line.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum SellPhase {
    /// Between `recGblGetTimeStamp` and `checkAlarms`, where dfanout's C reads
    /// it (`dfanoutRecord.c:126`).
    BeforeAlarms,
    /// The top of the multi-output dispatch, which is the routine fanout's and
    /// seq's C read it in.
    OutputDispatch,
}

/// What [`PvDatabase::dispatch_multi_output`] did this cycle.
///
/// `went_async` is C's `prec->pact = TRUE; return` (`seqRecord.c:143-196`):
/// the record's own cycle stops here, its groups run later on the callback
/// chain, and `asyncFinish` — our `complete_async_record` — runs the alarm /
/// monitor / FLNK epilogue when the last group is done. The caller MUST NOT
/// commit the cycle when this is set.
#[derive(Default)]
pub(crate) struct MultiOutDispatch {
    /// The pending `(stat, sevr)` the C record raises alongside its puts, for
    /// the caller to fold into `nsev` before `recGblResetAlarms`.
    pub alarm: Option<(u16, AlarmSeverity)>,
    /// The record armed a delayed group chain and is now PACT.
    pub went_async: bool,
}

impl MultiOutDispatch {
    fn went_async() -> Self {
        MultiOutDispatch {
            alarm: None,
            went_async: true,
        }
    }
}

/// **The one answer to "is this link target a record in this IOC?"** — the
/// locality question C's `dbInitLink` asks once (`dbLink.c:118-130`), together
/// with the handle that answer implies, taken in the SAME lookup.
///
/// Asking `has_name_no_resolve` and then `get_record` hashes one name twice,
/// and the pair can disagree: nothing holds the records map across the two
/// calls, so a record removed in the gap reads local on the first and `None`
/// on the second. The three-way result below is what that pair was spelling
/// out, with the record already in hand.
/// What [`PvDatabase::read_db_target_by_name`] found behind a link target.
enum TargetValue {
    /// A local record, its field as a reader sees it, the cell it came from,
    /// and — for a link that inherits it — the alarm the record held under
    /// the same lock hold, so a caller that wants the source's committed
    /// alarm neither looks the record up nor locks it a second time.
    Record(Option<EpicsValue>, SourceId, Option<LinkAlarm>),
    /// Local, but no record answers: a simple PV under the target's full
    /// `record[.FIELD]` name, or nothing readable at all.
    NoRecord(Option<EpicsValue>),
    /// Not served here. C `dbInitLink` makes it a CA link; the caller names it.
    External,
}

pub(super) enum LinkTarget {
    /// A record in this IOC — C's `dbDbLink` arm, with the cell it addresses.
    Local(Arc<RecordCell>),
    /// A name this IOC serves that is NOT a record (a simple PV). Local, so C
    /// would not make it a CA link, yet there is no record to read a field,
    /// a timestamp or metadata from.
    LocalNotRecord,
    /// Not served here at all: C `dbInitLink` falls through to `dbCaAddLink`.
    External,
}

/// The render a string read of `field` takes. Cold: only a `DBR_STRING`
/// read of a non-string field asks for it, and inlined into the read path
/// it cost every numeric link read.
#[cold]
#[inline(never)]
fn link_source_render(
    instance: &crate::server::record::RecordInstance,
    field: &str,
    v: &EpicsValue,
) -> crate::server::record::record_instance::SourceRender {
    instance.source_render_of(field, v)
}

impl PvDatabase {
    /// See [`LinkTarget`]. The records map is asked FIRST — a link target is
    /// overwhelmingly a record, and only the miss then pays the `simple_pvs`
    /// mutex `has_name_no_resolve` takes on every call.
    pub(super) fn link_target(&self, record: &[u8]) -> LinkTarget {
        if let Some(rec) = self.get_record(record) {
            return LinkTarget::Local(rec);
        }
        // A record was answered above.
        if self.has_name_no_resolve(record) {
            return LinkTarget::LocalNotRecord;
        }
        LinkTarget::External
    }

    /// C's `dbAddr` for `db`: resolved by name once per name-map revision
    /// and held in the link's `addr` cell ([`DbLink`](crate::server::record::DbLink)), so the cycle's read
    /// hashes no name. `None` for a target that is not a local record.
    pub(crate) fn db_link_addr(
        &self,
        db: &crate::server::record::DbLink,
    ) -> Option<crate::server::record::record_instance::ResolvedTarget> {
        match self.db_link_answer(db) {
            crate::server::record::DbLinkAnswer::Local(at) => Some(at),
            _ => None,
        }
    }

    /// The text route of `link`'s `$` address on a local link field or
    /// attribute ([`FieldAddr::text_route`]), which every read of it is
    /// answered through.
    ///
    /// [`FieldAddr::text_route`]: crate::server::record::record_instance::FieldAddr::text_route
    #[inline]
    pub(crate) fn text_route_of(
        &self,
        link: &crate::server::record::ParsedLink,
    ) -> Option<super::char_view::TextRoute> {
        match link {
            crate::server::record::ParsedLink::Db(db) if db.target().string_view => {
                self.db_link_addr(db)?.put.field().text_route()
            }
            _ => None,
        }
    }

    /// [`Self::db_link_addr`] with the non-local answers kept: a shadowing
    /// simple PV is settled once per revision like an address, so its read
    /// takes no `simple_pvs` lock and builds no name.
    pub(crate) fn db_link_answer(
        &self,
        db: &crate::server::record::DbLink,
    ) -> crate::server::record::DbLinkAnswer {
        use crate::server::record::record_instance::LinkTargetResolver;
        db.answer(self.name_revision(), || self.resolve_db_link(db))
    }

    /// [`Self::db_link_addr`] for the reader's own `INP`, lent with the link
    /// for as long as the reader is held: no lock and no refcount on a hit.
    /// The hit is two loads, so it is taken in the caller's frame.
    ///
    /// The reader is taken `&mut`: a miss publishes outside the cell's `lent`
    /// lock, which is sound only while no other borrow of the reader can
    /// reach the same cell.
    #[inline(always)]
    pub(crate) fn inp_addr<'g>(
        &self,
        instance: &'g mut crate::server::record::RecordInstance,
    ) -> Option<(
        &'g crate::server::record::ParsedLink,
        &'g crate::server::record::record_instance::ResolvedTarget,
    )> {
        use crate::server::record::record_instance::LinkTargetResolver;
        instance.lend_inp_addr(self.name_revision(), |db| self.resolve_db_link(db))
    }

    fn resolve_db_link(
        &self,
        db: &crate::server::record::DbLink,
    ) -> crate::server::record::DbLinkAnswer {
        use crate::server::record::DbLinkAnswer;
        // A simple PV spelling the target shadows it, as in `put_pv` and
        // `get_pv`.
        if let Some(pv) = self.shadowing_pv(db) {
            return DbLinkAnswer::Shadowed(pv);
        }
        let Some((name, rec)) = self.lookup_record(&db.target().record) else {
            return if self.has_name_no_resolve(&db.target().record) {
                DbLinkAnswer::Absent
            } else {
                DbLinkAnswer::External
            };
        };
        if !self.names_field(&rec, db.field_as_written()) {
            return DbLinkAnswer::Absent;
        }
        use crate::server::record::record_instance::PutAddr;
        let field = &db.target().field;
        let put = if db.target().string_view {
            match PutAddr::resolve_chars_in(&rec, field) {
                Some(put) => put,
                None => return DbLinkAnswer::Absent,
            }
        } else {
            PutAddr::resolve_in(&rec, field)
        };
        DbLinkAnswer::Local(crate::server::record::record_instance::ResolvedTarget {
            f64_slot: (db.target_is_val() && db.target().json_suffix.is_none())
                .then(|| super::field_io::double_slot(put.field()))
                .flatten(),
            rec,
            name,
            put,
        })
    }

    /// [`Self::link_target`] for a DB link, through its cached answer.
    fn db_link_target(&self, db: &crate::server::record::DbLink) -> LinkTarget {
        use crate::server::record::DbLinkAnswer;
        match self.db_link_answer(db) {
            DbLinkAnswer::Local(at) => LinkTarget::Local(at.rec),
            DbLinkAnswer::External => LinkTarget::External,
            DbLinkAnswer::Shadowed(_) | DbLinkAnswer::Absent => {
                self.link_target(&db.target().record)
            }
        }
    }

    /// A DB link's target read by name, for a link whose answer settled
    /// `Absent`.
    fn read_db_target_by_name(&self, db: &crate::server::record::DbLink) -> TargetValue {
        use crate::server::record::DbLinkAnswer;
        let target = db.target();
        match self.link_target(target.record.as_bytes()) {
            // The record was added after the answer settled `Absent`, so the
            // revision moved: read through the re-resolved answer, once. A
            // target gone again by then has nothing readable this cycle.
            LinkTarget::Local(_) => match self.db_link_answer(db) {
                DbLinkAnswer::Local(at) => self.target_value_of(&at.rec, db, at.put.field()),
                DbLinkAnswer::Shadowed(pv) => TargetValue::NoRecord(Some(pv.get())),
                DbLinkAnswer::Absent | DbLinkAnswer::External => TargetValue::NoRecord(None),
            },
            // A simple PV under the target's own name — the value `get_pv`
            // answers with before it ever reaches a record.
            LinkTarget::LocalNotRecord => {
                TargetValue::NoRecord(self.get_pv(local_name(&target.record, &target.field)).ok())
            }
            LinkTarget::External => TargetValue::External,
        }
    }

    #[inline(always)]
    fn target_value_of(
        &self,
        rec: &RecordCell,
        db: &crate::server::record::DbLink,
        addr: crate::server::record::record_instance::FieldAddr,
    ) -> TargetValue {
        let (value, alarm) = self.read_field_of(rec, db, addr);
        TargetValue::Record(value, SourceId::of(rec), alarm)
    }

    /// The read through a cached target, packed as the caller consumes it:
    /// the filtered value and, for an inheriting link, the source's alarm.
    /// Both [`Self::read_link_value_and_alarm`]'s hit and
    /// [`Self::read_db_link_at`] are this, so the two cannot disagree.
    /// `held` is the reader's set, which a target merged into it rides.
    /// `render` asks for the [`SourceRender`] a `DBR_STRING` conversion of the
    /// value needs, taken under this same hold.
    ///
    /// [`SourceRender`]: crate::server::record::record_instance::SourceRender
    // Off the debug FLNK frame; see `process_cycle`.
    #[cfg_attr(not(debug_assertions), inline(always))]
    fn read_db_at(
        &self,
        held: Option<&crate::server::database::SetGuard>,
        db: &crate::server::record::DbLink,
        at: &crate::server::record::record_instance::ResolvedTarget,
        render_requested: bool,
    ) -> RenderedRead {
        // A shadowed target has no address (`resolve_db_link`).
        let target = db.target();
        // Built in the shape it is returned in, and filtered in place: each
        // repack of the value on the way up is a copy the optimiser keeps —
        // which is also why a `$` address does not return early, and hands
        // its hold to the cold build instead.
        let instance = match held {
            Some(set) => at.rec.read_in(set),
            None => at.rec.read(),
        };
        let mut value;
        let alarm;
        let render;
        if at.put.field().char_view() {
            alarm = db_link_alarm(db, at, &instance);
            value = char_image_after_hold(instance, &target.field);
            render = None;
        } else {
            value = self.read_field_at(&instance, &target.field, at.put.field());
            alarm = db_link_alarm(db, at, &instance);
            // A `$` source is a `DBF_CHAR` array, which renders with nothing
            // of the record's; a string renders as itself.
            render = match &value {
                Some(v)
                    if render_requested
                        && !target.string_view
                        && !matches!(v, EpicsValue::String(_) | EpicsValue::StringArray(_)) =>
                {
                    Some(link_source_render(&instance, &target.field, v))
                }
                _ => None,
            };
            drop(instance);
        }
        if let Some(suffix) = target.json_suffix.as_deref() {
            value = value.map(|v| filter_read(suffix, v));
        }
        (value, alarm, render)
    }

    /// The read itself: `rec`'s field at `addr` — `None` when it resolves but
    /// does not read — with the alarm the record held under the same lock
    /// hold.
    ///
    /// The alarm is taken only for an `MS`/`MSS`/`MSI` link: C's
    /// `recGblInheritSevrMsg(pvlOptNMS)` is a `break`, so an NMS read has no
    /// use for it.
    #[inline(always)]
    fn read_field_of(
        &self,
        rec: &RecordCell,
        db: &crate::server::record::DbLink,
        addr: crate::server::record::record_instance::FieldAddr,
    ) -> (Option<EpicsValue>, Option<LinkAlarm>) {
        let instance = rec.read();
        let alarm = (db.monitor_switch != crate::server::record::MonitorSwitch::NoMaximize)
            .then(|| LinkAlarm::committed(&instance.common));
        let value = if addr.char_view() {
            char_image_after_hold(instance, &db.target().field)
        } else {
            self.read_field_at(&instance, &db.target().field, addr)
        };
        (value, alarm)
    }

    /// `get_pv`'s own first question, and NOT redundant with the records
    /// map: `check_name_free` keeps the three namespaces disjoint, so a name
    /// the map answers cannot also be a simple PV — but a simple PV may be
    /// registered under a name that SPELLS a record's field (`REC.FLD`), and
    /// that one shadows the field. The `VAL` spelling, where the two names
    /// are the same string, cannot. Asked once per name revision, by
    /// [`Self::resolve_db_link`].
    fn shadowing_pv(
        &self,
        db: &crate::server::record::DbLink,
    ) -> Option<Arc<crate::server::pv::ProcessVariable>> {
        if db.target_is_val() {
            return None;
        }
        let target = db.target();
        self.simple_pv_bytes(&local_name(&target.record, &target.field))
    }

    /// Read a `Db`-variant link's value honoring C `dbInitLink`'s
    /// locality rule (`dbLink.c:118-128`, the call being `dbCaAddLink` at
    /// `:128` — see [`PvDatabase::setup_external_link_opens`]): a PV link
    /// whose target record exists in this IOC reads from the local
    /// database; a non-local target is a CA link — `dbDbInitLink` fails to
    /// resolve it locally and falls through to `dbCaAddLink`, so its value
    /// comes
    /// from the ca link set. Pre-fix every `Db` arm read only the
    /// local DB (`get_pv`) and returned `None` for a non-local target, so
    /// a plain `INP="other:pv"` (no modifier) and a re-parsed multi-input
    /// (`INPA`..`INPL`) / `DOL` link never read the remote value.
    ///
    /// This is the single owner of that rule for the value-read path, so
    /// it holds uniformly — for every link field and regardless of an
    /// explicit `CP`/`CPP`/`CA` modifier — not only the
    /// `INP`/`OUT`/`TSEL`/`SDIS` parse caches the iocInit CP scan
    /// rewrites (the per-cycle re-parsed links have no cache to rewrite,
    /// so an init-time conversion can never reach them). The external
    /// read routes through the lset's lazy-open path: the first read
    /// opens the CA link and returns `None` until the monitor connects,
    /// then serves the cached value — exactly C `dbCaGetLink`.
    /// C `dbGetLink(link, read_as, ...)` from inside `init_record`. A CA link
    /// is not connected yet at init, nor is a DB link to a name no record
    /// here serves (`dbInitLink` made it CA); every other link reads as it
    /// would in a cycle.
    pub(super) fn read_link_at_init(
        &self,
        reader: &Arc<RecordCell>,
        link: &crate::server::record::ParsedLink,
        read_as: crate::server::record::LinkReadAs,
    ) -> crate::server::record::InitRead {
        use crate::server::recgbl::simm::LinkFetch;
        use crate::server::record::{InitGetFailure, InitRead, ParsedLink};
        let failed = |failure| InitRead {
            fetch: LinkFetch::Failed,
            inherit: None,
            failure,
        };
        match link {
            _ if crate::server::recgbl::simm::has_no_lset(link) => failed(InitGetFailure::NoLset),
            ParsedLink::Ca(_) | ParsedLink::Pva(_) | ParsedLink::PvaJson(_) => {
                failed(if self.get_failure_disconn(link) {
                    InitGetFailure::Disconn
                } else {
                    InitGetFailure::LinkAlarm
                })
            }
            ParsedLink::Db(db)
                if matches!(self.link_target(&db.target().record), LinkTarget::External) =>
            {
                failed(InitGetFailure::LinkAlarm)
            }
            _ => {
                // A PP source processes as C `dbDbGetValue`'s `dbScanPassive`
                // does from `init_record`, holding the reader's PACT.
                let (fetch, alarm) = self.read_link_value_as_from(
                    Some(reader),
                    None,
                    link,
                    read_as,
                    &mut ProcStack::new(),
                );
                InitRead {
                    fetch,
                    inherit: alarm.and_then(|alarm| {
                        self.inherit_source_alarm(Some(SourceId::of(reader)), link, alarm)
                    }),
                    failure: InitGetFailure::LinkAlarm,
                }
            }
        }
    }

    /// A link's value and the source's alarm off the SAME read —
    /// C `dbGetLink`, whose `dbDbGetValue` tail folds the source's severity in
    /// from the cell the value was just read from (`dbDbLink.c:220-232`).
    ///
    /// Asking the two separately — a value read, then
    /// [`Self::read_link_with_alarm`] with its value thrown away — read the
    /// target TWICE per cycle per link: two name resolutions and two value
    /// reads where C resolves nothing and reads once through its `dbAddr`. It
    /// also tore, since the severity then came off a later read than the value
    /// it was reported with.
    ///
    /// `render` asks for the source's
    /// [`SourceRender`](crate::server::record::record_instance::SourceRender),
    /// taken under the read's own hold.
    pub(crate) fn read_link_value_and_alarm_at_source(
        &self,
        src: Option<&Arc<RecordCell>>,
        link: &crate::server::record::ParsedLink,
        render: bool,
        visited: &mut ProcStack,
    ) -> RenderedRead {
        // PP: process a Passive DB source before reading, threading the
        // caller's chain state so an A<->B cycle terminates at the existing
        // guard — `read_link_value`'s rule, unchanged.
        // C `dbDbGetValue` returns `dbScanPassive`'s status unread.
        if let crate::server::record::ParsedLink::Db(db) = link
            && self.process_passive_db_source(src, db, visited).is_err()
        {
            return (None, None, None);
        }
        let (value, alarm, render) = self.read_link_value_and_alarm(link, src, render);
        // A failed read inherits NOTHING: the gate
        // [`Self::read_link_with_alarm_at`] applies, kept at the primitive
        // here too so the illegal "no value, but here is the source's
        // severity" pair cannot leave it.
        let alarm = match value {
            None if !crate::server::recgbl::simm::has_const_lset(link) => None,
            _ => alarm,
        };
        (value, alarm, render)
    }

    /// C `dbGetLink(plink, dbrType, ...)` — an input-link read for the DBR class
    /// the READER asked for ([`LinkReadAs`](crate::server::record::LinkReadAs)), not the source's native type.
    ///
    /// The typed READ seam, twin of the typed WRITE seam
    /// ([`Record::typed_output_buffer`](crate::server::record::Record::typed_output_buffer) + [`Self::resolve_out_target`]). C's
    /// conversion happens at the SOURCE (`dbConvert.c`'s
    /// `[field_type][dbrType]` table), which is why it belongs here and not at
    /// the reader's `put_field`: only this side can reach the source record's
    /// choice tables, so a `DBF_ENUM`/`DBF_MENU` source read as `DBR_STRING`
    /// delivers its state LABEL (`sseqRecord.c:644-661`) where a native read
    /// would hand over a bare index, and a `DBF_CHAR` array read as
    /// `DBF_CHAR` delivers the bytes it spells (`:682-686`) where a native read
    /// would hand over an array no numeric target can absorb.
    ///
    /// Returns C's `(status, buffer)` pair through the same
    /// [`LinkFetch`](crate::server::recgbl::simm::LinkFetch) classification [`Self::read_link_with_alarm`] uses — a
    /// CONSTANT link is [`LinkFetch::NoData`](crate::server::recgbl::simm::LinkFetch::NoData) (success, nothing written), a
    /// read or conversion that failed is [`LinkFetch::Failed`](crate::server::recgbl::simm::LinkFetch::Failed). The two used to
    /// be one `Option` here, which is how the `ReadDbLink` executor came to
    /// re-deliver a constant on every cycle while the multi-input fetch (same
    /// links, same records) correctly ignored it.
    /// The value at `read_as` and the source's alarm off the SAME read —
    /// see [`Self::read_link_value_and_alarm_at_source`].
    ///
    /// A conversion failure carries no alarm, which is where the separate-read
    /// caller already stood: it asked for the severity only inside its
    /// `LinkFetch::Value` arm.
    pub(crate) fn read_link_value_as_with_alarm(
        &self,
        src: Option<InFrame<'_>>,
        link: &crate::server::record::ParsedLink,
        read_as: crate::server::record::LinkReadAs,
        visited: &mut ProcStack,
    ) -> (crate::server::recgbl::simm::LinkFetch, Option<SourceAlarm>) {
        self.read_link_value_as_from(
            src.map(|src| src.unframed()),
            src.map(|src| src.set()),
            link,
            read_as,
            visited,
        )
    }

    /// [`Self::read_link_value_as_with_alarm`] for a reader `src` whose lock
    /// set `held` may not be taken — C's `psrc` either way.
    fn read_link_value_as_from(
        &self,
        src: Option<&Arc<RecordCell>>,
        held: Option<&super::SetGuard>,
        link: &crate::server::record::ParsedLink,
        read_as: crate::server::record::LinkReadAs,
        visited: &mut ProcStack,
    ) -> (crate::server::recgbl::simm::LinkFetch, Option<SourceAlarm>) {
        use crate::server::recgbl::simm::LinkFetch;
        let (value, alarm, render) = self.read_link_value_and_alarm_at_source(
            src,
            link,
            read_as.renders_at_source(),
            visited,
        );
        let Some(value) = value else {
            return (empty_read_fetch(link), None);
        };
        match self.apply_link_read_as(held, link, read_as, value, render) {
            Some(v) => (LinkFetch::Value(v), alarm),
            None => (LinkFetch::Failed, None),
        }
    }

    /// The `dbrType` conversion of one fetched link value — the single owner
    /// of C `dbGet`'s request-type switch for every framework input read
    /// (the pre-input [`Self::read_db_link_into_field`] stage, the single-INP
    /// soft fetch, the DOL fetch, the multi-input loop and the SIOL read).
    ///
    /// `None` is C's non-zero `dbGetLink` status (`dbConvert.c`'s NULL
    /// conversion slot), i.e. a FAILED read — never a silent no-op.
    #[inline(always)]
    ///
    /// `render` is what the read that produced `value` took of a local source
    /// under its own hold ([`RenderedRead`]): a `DBR_STRING` conversion never
    /// goes back to the source for it.
    pub(crate) fn apply_link_read_as(
        &self,
        held: Option<&crate::server::database::SetGuard>,
        link: &crate::server::record::ParsedLink,
        read_as: crate::server::record::LinkReadAs,
        value: EpicsValue,
        render: Option<crate::server::record::record_instance::SourceRender>,
    ) -> Option<EpicsValue> {
        match read_as {
            crate::server::record::LinkReadAs::Native => Some(value),
            _ => self.convert_link_read(held, link, read_as, value, render),
        }
    }

    /// [`Self::apply_link_read_as`]'s converting arms, out of the native
    /// read's frame.
    fn convert_link_read(
        &self,
        held: Option<&crate::server::database::SetGuard>,
        link: &crate::server::record::ParsedLink,
        read_as: crate::server::record::LinkReadAs,
        value: EpicsValue,
        render: Option<crate::server::record::record_instance::SourceRender>,
    ) -> Option<EpicsValue> {
        use crate::server::record::LinkReadAs;
        if let Some(route) = self.text_route_of(link) {
            return text_route_read(route, read_as, value);
        }
        match read_as {
            LinkReadAs::Native => Some(value),
            // C `dbGetLink(..., DBR_DOUBLE, ...)`: an array source contributes
            // its element 0 (C asks for one element, so `dbGet` converts offset
            // 0), the same rule the multi-input fetch applies.
            LinkReadAs::Double => link_value_as_f64(value).map(EpicsValue::Double),
            LinkReadAs::Char
            | LinkReadAs::UChar
            | LinkReadAs::Short
            | LinkReadAs::Float
            | LinkReadAs::Long
            | LinkReadAs::ULong
            | LinkReadAs::UShort
            | LinkReadAs::Int64
            | LinkReadAs::UInt64 => link_value_get_as(value, read_as.dbr_type()?),
            LinkReadAs::Enum => self.enum_read_of(held, link, value),
            LinkReadAs::String => self
                .dbr_string_of(link, value, render)
                .map(EpicsValue::String),
            LinkReadAs::CharArrayAsString { max_elements } => {
                char_bytes_as_string(&value, max_elements).map(EpicsValue::String)
            }
            LinkReadAs::Array { dbr, n_request } => {
                self.array_read_of(held, link, dbr, n_request, value, render)
            }
        }
    }

    /// [`LinkReadAs::Array`](crate::server::record::LinkReadAs::Array): an array source keeps its first `n_request`
    /// elements, each converted to `dbr`; a source of one element converts as
    /// the matching scalar request does, `dbGet`'s `no_elements == 1` test
    /// (`dbAccess.c:967`).
    fn array_read_of(
        &self,
        held: Option<&crate::server::database::SetGuard>,
        link: &crate::server::record::ParsedLink,
        dbr: DbFieldType,
        n_request: usize,
        mut value: EpicsValue,
        render: Option<crate::server::record::record_instance::SourceRender>,
    ) -> Option<EpicsValue> {
        if value.is_array() && value.count() != 1 {
            if dbr == DbFieldType::String {
                return self.dbr_strings_of(value, n_request, render);
            }
            value.truncate(n_request);
            return value.get_convert(dbr).ok();
        }
        match dbr {
            DbFieldType::String => self
                .dbr_string_of(link, value, render)
                .map(EpicsValue::String),
            DbFieldType::Enum => self.enum_read_of(held, link, value),
            _ => element_0(value)?.get_convert(dbr).ok(),
        }
    }

    /// The `DBR_ENUM` form of element 0 of a value just read from `link`,
    /// `dbGet`'s scalar convert: a string is
    /// [`cvt_st_e`](crate::server::record::cvt_st_e) against the SOURCE
    /// record, and an empty source is `S_db_onlyOne`.
    fn enum_read_of(
        &self,
        held: Option<&crate::server::database::SetGuard>,
        link: &crate::server::record::ParsedLink,
        value: EpicsValue,
    ) -> Option<EpicsValue> {
        let value = element_0(value)?;
        if let Some(s) = crate::server::record::cvt_st_e_input(&value) {
            let crate::server::record::ParsedLink::Db(db) = link else {
                return None;
            };
            let at = self.db_link_addr(db)?;
            let source = match held {
                Some(set) => at.rec.read_in(set),
                None => at.rec.read(),
            };
            let index = crate::server::record::cvt_st_e(&*source.record, s);
            drop(source);
            // C reports under the reader's lock set (`dbFastLinkConv.c:255-256`,
            // `:277-280`); printed here once the source's data lock is down.
            return match index {
                Ok(i) => Some(EpicsValue::Enum(i)),
                Err(report) => {
                    let field = at.put.field().desc.map_or("VAL", |d| d.name);
                    self.emit_put_report(&at.name, field, *report);
                    None
                }
            };
        }
        value.get_convert(DbFieldType::Enum).ok()
    }

    /// The `DBR_STRING` form of a value just read from `link`: rendered by the
    /// SOURCE record when the link is local (its choice tables are what turn an
    /// `ENUM`/`MENU` index into a state label — C `getEnumString`), by the value
    /// itself otherwise (an external link / constant / lnkCalc result carries no
    /// reachable field metadata).
    ///
    /// `value` is the read itself: only an index goes back to the source, for
    /// its choice table at the cached address, so a cycle resolves no name.
    ///
    /// The result is C's `MAX_STRING_SIZE` buffer: at most 39 bytes, as
    /// `getStringString` copies (`dbConvert.c`), whatever the source holds.
    fn dbr_string_of(
        &self,
        link: &crate::server::record::ParsedLink,
        value: EpicsValue,
        render: Option<crate::server::record::record_instance::SourceRender>,
    ) -> Option<PvString> {
        self.dbr_string_rendered(link, value, render)
            .map(crate::types::codec::cap_dbr_string)
    }

    /// [`Self::dbr_string_of`] for every element of an array source: `dbGet`
    /// converts `nRequest` elements at the source, so a float field renders
    /// at ITS precision (`getFloatString`), not the reader's.
    fn dbr_strings_of(
        &self,
        mut value: EpicsValue,
        n_request: usize,
        render: Option<crate::server::record::record_instance::SourceRender>,
    ) -> Option<EpicsValue> {
        let rendered = match render {
            Some(render) => render.dbr_strings(value, n_request)?,
            None => {
                value.truncate(n_request);
                value.get_convert(DbFieldType::String).ok()?
            }
        };
        Some(crate::types::codec::cap_dbr_strings(rendered))
    }

    fn dbr_string_rendered(
        &self,
        link: &crate::server::record::ParsedLink,
        value: EpicsValue,
        render: Option<crate::server::record::record_instance::SourceRender>,
    ) -> Option<PvString> {
        let value = match value {
            EpicsValue::String(s) => return Some(s),
            value => value,
        };
        // A string field's `$`, or a remote one, is a `DBF_CHAR` array:
        // `getCharString` of element 0, with no field metadata to apply. A
        // link's or attribute's is read from its text ahead of this.
        if let crate::server::record::ParsedLink::Db(db) = link
            && db.target().string_view
        {
            return crate::server::record::value_as_dbr_string(&element_0(value)?);
        }
        if let Some(render) = render {
            return render.dbr_string(value);
        }
        let value = element_0(value)?;
        // An external ENUM read as `DBR_STRING` is the server's string, off
        // the lset's `DBR_STRING` monitor; C `dbCaGetLink` fails the read
        // until that monitor's first event (`dbCa.c:436-454`). An lset with
        // no such monitor leaves the index digits.
        if let EpicsValue::Enum(_) = value
            && let Some(external) = link.external()
        {
            match self.external_enum_string(external) {
                super::EnumString::Got(s) => return Some(s),
                super::EnumString::Pending => return None,
                super::EnumString::NotServed => {}
            }
        }
        crate::server::record::value_as_dbr_string(&value)
    }

    /// lnkCalc evaluation: resolve each input, bind the calc engine's
    /// variable slots by position, run `expr`, return the result as
    /// `EpicsValue::Double`. Returns `None` if expr compile fails or eval
    /// fails — the caller treats the link as unresolvable.
    pub fn evaluate_calc_link(&self, calc: &crate::server::record::CalcLink) -> Option<EpicsValue> {
        self.read_calc_link(calc, None).0
    }

    /// C `lnkCalc_getValue` (`lnkCalc.c:551-623`): the value, and the alarm
    /// its input reads, then `major`, else `minor`, raise on `reader` with
    /// `recGblSetSevrMsg` — folded to the one that sequence leaves, which
    /// [`Self::inherit_source_alarm`] applies.
    pub(crate) fn read_calc_link(
        &self,
        calc: &crate::server::record::CalcLink,
        reader: Option<&Arc<RecordCell>>,
    ) -> (Option<EpicsValue>, Option<SourceAlarm>) {
        use crate::server::record::AlarmSeverity;
        let Some((vars, mut raised)) = self.calc_link_args(calc, reader) else {
            return (None, None);
        };
        calc.val.set_post_sevr(AlarmSeverity::NoAlarm);
        // C `calcPerform(clink->arg, &val, expr)`: `VAL` reads `val`.
        let perform = |expr: &str, val: f64| {
            let compiled = crate::calc::compile(expr).ok()?;
            let mut inputs = crate::calc::engine::NumericInputs::with_vars(vars);
            inputs.prev_val = val;
            crate::calc::eval(&compiled, &mut inputs).ok()
        };
        // `clink->val` is the last result.
        let Some(val) = calc
            .expr
            .as_deref()
            .and_then(|e| perform(e, calc.val.get()))
        else {
            return (None, None);
        };
        calc.val.set(val);
        for (expr, sevr, msg) in [
            (&calc.major, AlarmSeverity::Major, "post_major error"),
            (&calc.minor, AlarmSeverity::Minor, "post_minor error"),
        ] {
            let Some(expr) = expr else { continue };
            match perform(expr, val) {
                None => return (None, None),
                Some(alval) if alval != 0.0 => {
                    calc.val.set_post_sevr(sevr);
                    let alarm = LinkAlarm {
                        stat: crate::server::recgbl::alarm_status::LINK_ALARM,
                        sevr,
                        amsg: msg.into(),
                    };
                    raise(&mut raised, alarm);
                    break;
                }
                Some(_) => {}
            }
        }
        let alarm = raised.map(|alarm| SourceAlarm {
            alarm,
            record: None,
        });
        (Some(EpicsValue::Double(val)), alarm)
    }

    /// The `dbGetLink` of each lnkCalc input into `clink->arg`
    /// (`lnkCalc.c:566-584`), and what those reads raise on `reader`,
    /// folded. A failed read leaves `arg[i]` as it was and the evaluation
    /// goes on. `None` past C's argument limit.
    fn calc_link_args(
        &self,
        calc: &crate::server::record::CalcLink,
        reader: Option<&Arc<RecordCell>>,
    ) -> Option<([f64; crate::calc::engine::CALC_NARGS], Option<LinkAlarm>)> {
        use crate::calc::engine::CALC_NARGS;
        use crate::server::record::CalcArg;
        // The parser already refuses an over-long `args`; a link built
        // in-process could still carry one, and C's limit is a refusal
        // everywhere it appears (`lnkCalc.c:135-139`).
        if calc.args.len() > CALC_NARGS {
            return None;
        }
        let tinp = calc.time_source.map(|t| usize::from(t as u8 - b'A'));
        let mut vars = [0.0f64; CALC_NARGS];
        let mut raised = None;
        for (i, arg) in calc.args.iter().enumerate() {
            vars[i] = match arg {
                // Nothing reads over a literal: C's matching `inp[i]` is a
                // zeroed CONSTANT link and `dbGetLink` on it delivers
                // nothing (`dbConstLink.c:219-225`).
                CalcArg::Literal(n) => *n,
                CalcArg::Link(link) => {
                    let (value, alarm) = self.read_calc_input(link, reader);
                    if let Some(alarm) = alarm {
                        raise(&mut raised, alarm);
                    }
                    if let Some(value) = value {
                        calc.val.set_arg(i, value);
                        // `readLocked` (`lnkCalc.c:539-549`).
                        if tinp == Some(i)
                            && let Some((time, utag)) = self.db_get_time_stamp_tag(link)
                        {
                            calc.val.set_stamp(time, utag);
                        }
                    }
                    calc.val.arg(i)
                }
            };
            if tinp == Some(i)
                && let Some(reader) = reader
            {
                stamp_device_time(reader, calc.val.stamp());
            }
        }
        Some((vars, raised))
    }

    /// One lnkCalc input, C `dbGetLink(child, DBR_DOUBLE, &clink->arg[i])`
    /// on a child whose `precord` is the reader (`lnkCalc.c:458`): the value,
    /// `None` when the read failed, and what the read raises on the reader.
    /// It never processes the target. Every input is its own `dbInitLink`
    /// link, so the read carries the locality split, and a DB input the
    /// self-exclusion, of any other link.
    fn read_calc_input(
        &self,
        link: &crate::server::record::ParsedLink,
        reader: Option<&Arc<RecordCell>>,
    ) -> (Option<f64>, Option<LinkAlarm>) {
        // A constant delivers nothing over what `dbLoadLink` put in `arg[i]`
        // when the link opened (`lnkCalc.c:459`).
        if crate::server::recgbl::simm::has_const_lset(link) {
            return (None, None);
        }
        let (value, alarm, _) = self.read_link_value_and_alarm(link, reader, false);
        let Some(value) = value.and_then(link_value_as_f64) else {
            return (None, self.get_link_failure(link, CHILD_LINK_FIELD));
        };
        let alarm = alarm
            .and_then(|alarm| {
                self.inherit_source_alarm(reader.map(|r| SourceId::of(r)), link, alarm)
            })
            .and_then(|(ms, alarm)| {
                let (stat, sevr, amsg) = inherited(ms, &alarm)?;
                Some(LinkAlarm {
                    stat,
                    sevr,
                    amsg: amsg.cloned().unwrap_or_default(),
                })
            });
        (Some(value), alarm)
    }

    /// **The single owner of a process-time link read that carries both a
    /// value and an alarm** — the input-fetch readers (INPA..L, INAA..LL,
    /// SUBL, output-time DOL) go through here.
    ///
    /// Returns C's `(status, buffer)` pair, not an `Option`: a CONSTANT link is
    /// [`LinkFetch::NoData`](crate::server::recgbl::simm::LinkFetch::NoData) — SUCCESS with nothing delivered
    /// (`dbConstLink.c:219-225` `dbConstGetValue`: `*pnRequest = 0; return 0`)
    /// — which is NOT the same as [`LinkFetch::Failed`](crate::server::recgbl::simm::LinkFetch::Failed). Collapsing the two
    /// into `Option` is what made a constant input both re-apply its value on
    /// every cycle (destroying a client's `caput A=99` on a
    /// `field(INPA,"5")` calc) and, once it stopped delivering, look like a
    /// failed read that gates `fetch_values`. The constant reaches the record
    /// exactly once, at init, through
    /// [`super::PvDatabase::rec_gbl_init_constant_links`].
    ///
    /// `render` asks for the source's part of a `DBR_STRING` conversion, as
    /// [`Self::read_link_with_alarm_at`] takes it.
    pub(crate) fn read_link_with_alarm(
        &self,
        reader: Option<&Arc<RecordCell>>,
        link: &crate::server::record::ParsedLink,
        render: bool,
    ) -> RenderedFetch {
        self.read_link_with_alarm_at(reader, link, None, render)
    }

    /// [`Self::read_link_with_alarm`] with the link's local target already
    /// in hand — `at` is the record's cached handle for the link, validated
    /// by [`ParsedInputLink::target`](crate::server::record::record_instance::ParsedInputLink::target), and `None` resolves the
    /// name as before. `render` asks for the [`SourceRender`] a `DBR_STRING`
    /// conversion of the value takes from a local source, under the read's
    /// own hold.
    ///
    /// [`SourceRender`]: crate::server::record::record_instance::SourceRender
    ///
    /// `always`: each input read's cycle runs it, and the heuristic dropped
    /// it out of line once its body grew, at +29 instr/cycle on seq.
    #[inline(always)]
    pub(crate) fn read_link_with_alarm_at(
        &self,
        reader: Option<&Arc<RecordCell>>,
        link: &crate::server::record::ParsedLink,
        at: Option<&crate::server::record::record_instance::ResolvedTarget>,
        render: bool,
    ) -> RenderedFetch {
        use crate::server::recgbl::simm::LinkFetch;
        // C `dbDbGetValue` through the link's `dbAddr`: the target in hand
        // is read in one lock hold and classified here, straight into the
        // pair the caller wants — the by-name path below resolves and
        // repacks it through `TargetValue` first. A failed read inherits
        // nothing (the gate on the general path below); a simple-PV
        // shadow answers with no record, so no alarm; and an NMS link
        // inherits nothing either — C's `recGblInheritSevrMsg(pvlOptNMS)`
        // is a `break` — so its alarm is not carried to the fold that
        // would drop it.
        if let (crate::server::record::ParsedLink::Db(db), Some(at)) = (link, at) {
            return self.read_db_link_at(None, db, at, render);
        }
        let (value, alarm, render) = self.read_link_value_and_alarm(link, reader, render);
        let fetch = match value {
            Some(v) => LinkFetch::Value(v),
            None => empty_read_fetch(link),
        };
        // C gates the inheritance tail on the READ's own status, in all three
        // schemes: `if (!status && precord != dbChannelRecord(chan))`
        // (`dbDbLink.c:228`), `if (!status)` (`dbCa.c:500`), and pvxs
        // `pvaGetValue` returning `-1` at `pvxs/ioc/pvalink_lset.cpp:272` before its MS
        // gate at `:424-425`. A failed read inherits NOTHING; what it produces is
        // `setLinkAlarm` — LINK_ALARM/INVALID with the LINK FIELD's name — which
        // is a different effect and ignores the MS class.
        //
        // The gate lives here, at the primitive, and not at each application
        // site, so the illegal `(Failed, Some(alarm))` pair cannot be built: the
        // deferred readers hand the alarm to their caller and would each need
        // the same test.
        //
        // Which path SHOWED the defect is decided by the order the two
        // equal-INVALID effects arrive in, since `rec_gbl_set_sevr` is
        // strict-greater and keeps the first. The deferred stage
        // (`read_db_link_into_field`, `processing.rs:4831`) raises
        // `setLinkAlarm` before folding `link_alarms` (`:3249`), so the wrong
        // status arrived second and was dropped — that path was MASKED. The
        // inline reader is the other way round: `inherit_link_severity`
        // (`:6109`) runs inside the read and `db_get_link`'s `setLinkAlarm`
        // (`:5952`) only after, so the source's status landed first and stuck.
        // swait's `recDynLinkGet` has no `setLinkAlarm` at all, so it published
        // the source's status outright.
        let alarm = match fetch {
            LinkFetch::Failed => None,
            _ => alarm,
        };
        (fetch, alarm, render)
    }

    /// The cached-target arm of [`Self::read_link_with_alarm_at`], in its
    /// own frame so the multi-input loop — the one caller that always has
    /// the target in hand — takes it inline and the value lands where it is
    /// consumed, not through the general path's frame and back. `always`,
    /// because the general path is a second caller and a plain hint loses
    /// the loop's copy to it.
    #[inline(always)]
    pub(crate) fn read_db_link_at(
        &self,
        held: Option<&crate::server::database::SetGuard>,
        db: &crate::server::record::DbLink,
        at: &crate::server::record::record_instance::ResolvedTarget,
        render: bool,
    ) -> RenderedFetch {
        use crate::server::recgbl::simm::LinkFetch;
        match self.read_db_at(held, db, at, render) {
            (Some(v), alarm, render) => (LinkFetch::Value(v), alarm, render),
            (None, ..) => (LinkFetch::Failed, None, None),
        }
    }

    /// The read of `link`'s cached target at the reader's request, for a
    /// caller that has the target in hand: the value as `read_as` asks, and
    /// the source's alarm only for a read and conversion that both
    /// succeeded (`dbDbLink.c:228`), as [`Self::read_link_value_as_with_alarm`]
    /// gates the general path. `None` with no alarm for a non-DB `link`.
    // Off the debug FLNK frame; see `process_cycle`.
    #[cfg_attr(not(debug_assertions), inline(always))]
    pub(crate) fn read_db_link_as_at(
        &self,
        held: &crate::server::database::SetGuard,
        link: &crate::server::record::ParsedLink,
        at: &crate::server::record::record_instance::ResolvedTarget,
        read_as: crate::server::record::LinkReadAs,
    ) -> (Option<EpicsValue>, Option<SourceAlarm>) {
        let crate::server::record::ParsedLink::Db(db) = link else {
            return (None, None);
        };
        if read_as == crate::server::record::LinkReadAs::Double {
            let (value, alarm) = self.read_db_link_f64_at(held, db, at);
            return (value.map(EpicsValue::Double), alarm);
        }
        let (Some(value), alarm, render) =
            self.read_db_at(Some(held), db, at, read_as.renders_at_source())
        else {
            return (None, None);
        };
        match self.apply_link_read_as(Some(held), link, read_as, value, render) {
            Some(value) => (Some(value), alarm),
            None => (None, None),
        }
    }

    /// [`Self::read_db_link_at`] read as `DBR_DOUBLE`: `None`, with no alarm,
    /// for a read or a conversion that failed — C inherits only on
    /// `dbGetLink`'s success (`dbDbLink.c:228`), and a failed conversion is a
    /// failed status. An unfiltered `VAL` the record serves as `DBF_DOUBLE`
    /// is read straight off its slot, the `dbGetLink` of C's `dbAddr`.
    // Off the debug FLNK frame; see `process_cycle`.
    #[cfg_attr(not(debug_assertions), inline(always))]
    pub(crate) fn read_db_link_f64_at(
        &self,
        held: &crate::server::database::SetGuard,
        db: &crate::server::record::DbLink,
        at: &crate::server::record::record_instance::ResolvedTarget,
    ) -> (Option<f64>, Option<SourceAlarm>) {
        if let Some(slot) = at.f64_slot {
            let instance = at.rec.read_in(held);
            let Some(value) = instance.record.get_slot_f64(slot) else {
                return (None, None);
            };
            let alarm = (db.monitor_switch != crate::server::record::MonitorSwitch::NoMaximize)
                .then(|| {
                    SourceAlarm::local(
                        SourceId::of(&at.rec),
                        LinkAlarm::committed(&instance.common),
                    )
                });
            return (Some(value), alarm);
        }
        let (Some(v), alarm, _) = self.read_db_at(Some(held), db, at, false) else {
            return (None, None);
        };
        let f = match at.put.field().text_route() {
            Some(route) => text_route_read(route, crate::server::record::LinkReadAs::Double, v)
                .and_then(|v| v.to_f64()),
            None => link_value_as_f64(v),
        };
        match f {
            Some(f) => (Some(f), alarm),
            None => (None, None),
        }
    }

    /// **The single owner of input-link severity inheritance** — C
    /// `dbDbGetValue`'s tail (`dbDbLink.c:228-232`), which EVERY healthy
    /// `dbGetLink` on a DB link runs:
    ///
    /// ```c
    /// if (!status && precord != dbChannelRecord(chan))
    ///     recGblInheritSevrMsg(plink->value.pv_link.pvlMask & pvlOptMsMode,
    ///         plink->precord, dbChannelRecord(chan)->stat,
    ///         dbChannelRecord(chan)->sevr, dbChannelRecord(chan)->amsg);
    /// ```
    ///
    /// Given the reader, the link it just read and the source alarm that read
    /// produced, it returns the `(MS class, source alarm)` pair the reader owes
    /// its PENDING alarm — or `None` when C inherits nothing. Callers hand the
    /// pair to [`inherit_sevr_msg`]; no caller decides for itself which links
    /// inherit, which is how the `ReadDbLink` path came to drop MS entirely.
    ///
    /// Two rules live here and nowhere else:
    ///
    /// * **Self-exclusion** — C's `precord != dbChannelRecord(chan)` guard. A
    ///   link that reads the reader's OWN field must not fold the reader's
    ///   committed severity back into its pending one: `rec_gbl_reset_alarms`
    ///   would re-commit it next cycle, so a single MAJOR would latch forever.
    ///   Alias-aware, because C compares record pointers.
    /// * **MS class per scheme** — DB and CA links carry their own parsed
    ///   `MonitorSwitch`; a PVA link's lset has already applied the MS/NMS/MSI
    ///   gate, so its (already final) severity folds as `MaximizeStatus` to keep
    ///   the remote stat + message. A calc link's alarm is the one its
    ///   `getValue` raised with `recGblSetSevrMsg`, which `MaximizeStatus` is.
    ///   Constant/Hw links inherit nothing.
    ///
    /// No alarm — an NMS read, which never takes one — is answered in the
    /// caller's frame.
    #[inline(always)]
    pub(crate) fn input_link_inheritance(
        &self,
        reader: InFrame<'_>,
        link: &crate::server::record::ParsedLink,
        alarm: Option<SourceAlarm>,
    ) -> Option<(crate::server::record::MonitorSwitch, LinkAlarm)> {
        self.inherit_source_alarm(Some(SourceId::of_frame(reader)), link, alarm?)
    }

    /// [`Self::input_link_inheritance`] for a read that carried an alarm.
    /// `reader` is `None` for a read no record made.
    fn inherit_source_alarm(
        &self,
        reader: Option<SourceId>,
        link: &crate::server::record::ParsedLink,
        alarm: SourceAlarm,
    ) -> Option<(crate::server::record::MonitorSwitch, LinkAlarm)> {
        // NMS: `recGblInheritSevrMsg(pvlOptNMS)` is a `break`.
        let inherits = |ms| ms != crate::server::record::MonitorSwitch::NoMaximize;
        let SourceAlarm { alarm, record } = alarm;
        match link {
            crate::server::record::ParsedLink::Db(db) => {
                // C's `precord != dbChannelRecord(chan)`, asked as the pointer
                // test it is. The handle came off the same read that produced
                // the alarm, so there is no second lookup and no name to
                // canonicalise: an alias and the record it names ARE one cell.
                if record.is_some() && record == reader {
                    return None;
                }
                inherits(db.monitor_switch).then_some((db.monitor_switch, alarm))
            }
            crate::server::record::ParsedLink::Ca(ca) => {
                inherits(ca.monitor_switch).then_some((ca.monitor_switch, alarm))
            }
            crate::server::record::ParsedLink::Pva(_)
            | crate::server::record::ParsedLink::PvaJson(_)
            | crate::server::record::ParsedLink::Calc(_) => {
                Some((crate::server::record::MonitorSwitch::MaximizeStatus, alarm))
            }
            _ => None,
        }
    }

    /// [`Self::input_link_inheritance`] folded straight into the reader's
    /// pending alarm — C `recGblInheritSevrMsg` inside `dbGetLink`, for a
    /// caller that holds the reader.
    #[inline]
    pub(crate) fn fold_input_link_alarm(
        &self,
        common: &mut crate::server::record::CommonFields,
        reader: InFrame<'_>,
        link: &crate::server::record::ParsedLink,
        alarm: SourceAlarm,
    ) {
        if let Some((ms, alarm)) = self.input_link_inheritance(reader, link, Some(alarm)) {
            inherit_sevr_msg(common, ms, &alarm);
        }
    }

    /// The raw value+alarm read behind [`Self::read_link_with_alarm`]. Never
    /// call this directly from a process path — it cannot tell "constant" from
    /// "failed"; that is the classifier's job.
    #[inline]
    fn read_link_value_and_alarm(
        &self,
        link: &crate::server::record::ParsedLink,
        reader: Option<&Arc<RecordCell>>,
        render: bool,
    ) -> RenderedRead {
        if let crate::server::record::ParsedLink::Db(db) = link {
            match self.db_link_answer(db) {
                crate::server::record::DbLinkAnswer::Local(at) => {
                    return self.read_db_at(None, db, &at, render);
                }
                crate::server::record::DbLinkAnswer::Shadowed(pv) => {
                    return (Some(filtered_read(db.target(), pv.get())), None, None);
                }
                crate::server::record::DbLinkAnswer::External => {
                    let (value, alarm) = self.read_db_external(db);
                    return (value, alarm, None);
                }
                crate::server::record::DbLinkAnswer::Absent => {}
            }
        }
        let (value, alarm) = self.read_unresolved_link_value_and_alarm(link, reader);
        (value, alarm, None)
    }

    /// What a link's lset `getValue` raised on the reader itself before
    /// failing — pvalink's `"%s Disconn"` (`pvalink_lset.cpp:263-268`).
    /// Both `dbGetLink` and `dbTryGetLink` reach the lset, so a reader of
    /// either kind runs this first.
    #[cold]
    pub(crate) fn lset_get_failed(
        &self,
        common: &mut crate::server::record::CommonFields,
        link: &crate::server::record::ParsedLink,
        field: &'static str,
    ) {
        if self.get_failure_disconn(link) {
            crate::server::recgbl::rec_gbl_set_sevr_msg(
                common,
                crate::server::recgbl::alarm_status::LINK_ALARM,
                crate::server::record::AlarmSeverity::Invalid,
                crate::server::record::SparseText::link_disconn(field),
            );
        }
    }

    /// [`LinkSet::get_failure_disconn`](super::LinkSet::get_failure_disconn)
    /// of the lset `link` names by scheme. A bare name (a CA link) is
    /// `dbCa`'s, which raises nothing.
    pub(crate) fn get_failure_disconn(&self, link: &crate::server::record::ParsedLink) -> bool {
        let Some(external) = link.external() else {
            return false;
        };
        let (scheme, lref) = Self::external_link_ref(external);
        super::link_put_queue::find_lset(&self.inner, scheme, |lset| {
            lset.get_failure_disconn(lref).then_some(())
        })
        .is_some()
    }

    /// C `dbIsLinkConnected`: only an external link can be down, and its
    /// lset says whether it is.
    pub(crate) fn link_connected(&self, link: &crate::server::record::ParsedLink) -> bool {
        let Some(external) = link.external() else {
            return true;
        };
        let (target, lref) = Self::external_link_ref(external);
        super::link_put_queue::find_lset(&self.inner, target, |lset| {
            lset.is_connected(lref).then_some(())
        })
        .is_some()
    }

    /// C `dbCaGetLinkDBFtype` (`dbCa.c:665-675`) of an external link: the
    /// connected channel's type, `None` (C -1) while it is down.
    pub(crate) fn external_link_dbf_type(
        &self,
        link: &crate::server::record::ParsedLink,
    ) -> Option<DbFieldType> {
        self.external_link_metadata(link.external()?)?
            .dbf_type
            .map(link_dbf_to_field_type)
    }

    /// C `doResolveLinks`' `openLink` (`iocInit.c:535-559`) for each
    /// external link of `rec_arc`, once per record
    /// ([`RecordCell::claim_link_opens`]); a link its lset opens without one
    /// is bound by [`RecordInstance::bind_no_lset`], a forward link it opens
    /// gets its handle ([`Self::bind_external_forward`]). Runs ahead of the
    /// record's init reads.
    ///
    /// [`RecordCell::claim_link_opens`]: crate::server::record::RecordCell::claim_link_opens
    /// [`RecordInstance::bind_no_lset`]: crate::server::record::RecordInstance::bind_no_lset
    pub(crate) fn open_record_links(&self, rec_arc: &Arc<crate::server::record::RecordCell>) {
        if !rec_arc.claim_link_opens() {
            return;
        }
        let links = Self::installed_links(&rec_arc.read());
        let opened: Vec<(String, super::LinkOpen)> = links
            .into_iter()
            .filter_map(|(field, link)| {
                let holder = super::LinkHolder::new(
                    rec_arc.rdes().record_type,
                    rec_arc.name().as_bytes(),
                    &field,
                );
                let open = self.open_link(holder, &link)?;
                Some((field, open))
            })
            .collect();
        if opened.is_empty() {
            return;
        }
        let mut instance = rec_arc.write();
        for (field, open) in &opened {
            self.bind_opened(&mut instance, field, *open);
        }
    }

    /// `dbPutFieldLink`'s `dbAddLink` → `openLink` (`dbAccess.c:1208`,
    /// `dbLink.c:194-199`) for the link a put just stored in `field`, under
    /// the put's hold, after its `special()` pass 1 and before its posts.
    pub(crate) fn open_put_link(
        &self,
        instance: &mut crate::server::record::RecordInstance,
        field: &str,
    ) {
        let holder = super::LinkHolder::new(
            instance.record.record_type(),
            instance.name.as_bytes(),
            field,
        );
        let open = instance
            .stored_link_shared(field)
            .and_then(|link| self.open_link(holder, link));
        if let Some(open) = open {
            self.bind_opened(instance, field, open);
        }
    }

    /// `field`'s stored link ahead of a put that may replace it, when an lset
    /// holds it: what [`Self::remove_put_link`] compares the put's result to.
    pub(crate) fn link_before_put(
        instance: &crate::server::record::RecordInstance,
        field: &str,
    ) -> Option<Arc<crate::server::record::ParsedLink>> {
        instance
            .stored_link_shared(field)
            .filter(|link| link.external().is_some())
            .cloned()
    }

    /// `dbPutFieldLink`'s `dbRemoveLink` (`dbAccess.c:1167`) → the lset's
    /// `removeLink` for `before`, the link [`Self::link_before_put`] saw, once
    /// the put replaced it or left the field unadded. Under the put's hold,
    /// ahead of [`Self::open_put_link`], so an lset never holds two links for
    /// one holder.
    pub(crate) fn remove_put_link(
        &self,
        instance: &crate::server::record::RecordInstance,
        field: &str,
        before: Option<Arc<crate::server::record::ParsedLink>>,
    ) {
        let Some(before) = before else {
            return;
        };
        let replaced = instance
            .stored_link_shared(field)
            .is_none_or(|now| !Arc::ptr_eq(now, &before));
        if !replaced && !instance.link_unadded(field) {
            return;
        }
        let Some(external) = before.external() else {
            return;
        };
        let holder = super::LinkHolder::new(
            instance.record.record_type(),
            instance.name.as_bytes(),
            field,
        );
        self.remove_external_link(holder, external);
    }

    /// C `dbRemoveLink`'s `removeLink` of an external link
    /// (`dbLink.c:202-213`), a CA link's `dbCaRemoveLink` included
    /// (`dbCa.c:431-446`). The link reads as removed before its lset hears
    /// of it, and the queue forgets it, so an open in flight leaves nothing
    /// behind: `dbCaTask` serves `CA_CLEAR_CHANNEL` first (`dbCa.c:1200`).
    fn remove_external_link(
        &self,
        holder: super::LinkHolder<'_>,
        link: crate::server::record::ExternalRef<'_>,
    ) {
        link.open.remove();
        self.inner.link_puts.retire(link.open.serial());
        let (scheme, lref) = Self::external_link_ref(link);
        super::link_put_queue::find_lset(&self.inner, scheme, |lset| {
            lset.remove_link(holder, lref);
            Some(())
        });
    }

    /// C `doCloseLinks` over every record (`iocInit.c:665-704`), from
    /// `iocShutdown` before `initHookAfterCloseLinks`: under each record's
    /// hold its CA and JSON links are removed as `dbRemoveLink` removes them
    /// ([`Self::remove_put_link`]'s lset call, then the
    /// [`ParsedLink::Removed`](crate::server::record::ParsedLink::Removed)
    /// the record keeps), its dsxt device is closed, and a record either
    /// touched is left PACT, so nothing processes it again. An `isolated`
    /// build's DB links are removed too (`iocInit.c:680`); the lock sets
    /// are not split, as C's `dbDbRemoveLink` splits them (`dbDbLink.c:141`).
    pub(crate) fn close_links(&self, isolated: bool) {
        use crate::server::record::DbLinkType;
        let cells: Vec<Arc<super::RecordCell>> =
            self.inner.records.read().values().cloned().collect();
        for rec in cells {
            // An `epicsExit` from inside a record's processing reaches here
            // on the thread that is processing it. C locks that record again
            // and closes it; this port cannot alias the frame's borrow, so
            // the record is left as it is.
            let Some(mut inst) = rec.try_write() else {
                continue;
            };
            let mut locked = false;
            for (field, raw, ftype) in Self::link_field_texts(&inst) {
                let Some(link) = inst.stored_link_shared(&field).cloned() else {
                    continue;
                };
                match link.link_type_after_init(&raw, ftype) {
                    DbLinkType::CaLink | DbLinkType::JsonLink => {}
                    DbLinkType::DbLink if isolated => {}
                    _ => continue,
                }
                locked = true;
                if let Some(external) = link.external() {
                    let holder = super::LinkHolder::new(
                        inst.record.record_type(),
                        inst.name.as_bytes(),
                        &field,
                    );
                    self.remove_external_link(holder, external);
                }
                inst.bind_no_lset(&field);
            }
            locked |= inst.close_device();
            if locked {
                inst.enter_pact();
            }
        }
    }

    /// A record's own post of `field` under its hold: the put, then the open
    /// of the link it installed. C's `dbPut` refuses a link field
    /// (`dbAccess.c:1340`); only `dbPutFieldLink` changes one, and it opens it
    /// (`dbAccess.c:1208`), so a link this post installs is opened as a put's.
    pub(crate) fn put_internal_field_opening(
        &self,
        instance: &mut crate::server::record::RecordInstance,
        field: &str,
        value: EpicsValue,
        links: &mut crate::server::record::PreparedLinks,
    ) -> crate::error::CaResult<()> {
        let before = Self::link_before_put(instance, field);
        if let Some(link_field) = instance.put_internal_field_of(field, value, links)? {
            self.remove_put_link(instance, link_field, before);
            self.open_put_link(instance, link_field);
        }
        Ok(())
    }

    /// C `dbLinkOpen` (`dbLink.c:194-200`) of an init's or a put's `link`.
    fn open_link(
        &self,
        holder: super::LinkHolder<'_>,
        link: &crate::server::record::ParsedLink,
    ) -> Option<super::LinkOpen> {
        self.open_json_link(link);
        self.open_external_link(holder, link)
    }

    /// The `openLink` of a JSON link whose lset is base's own:
    /// `lnkState_open`'s `dbStateCreate` (`lnkState.c:110-116`), and
    /// `lnkCalc_open` (`lnkCalc.c:449-466`), which opens each child,
    /// `dbLoadLink`s it into `arg[i]` and opens `out`. Only `lnkConst` has a
    /// `loadScalar`, so no other child, and no failed load, changes `arg[i]`.
    fn open_json_link(&self, link: &crate::server::record::ParsedLink) {
        use crate::server::record::{CalcArg, ParsedLink, constant_load_scalar};
        use crate::types::c_parse::Converted;
        match link {
            ParsedLink::State(st) => {
                self.db_state(&st.name);
            }
            ParsedLink::Calc(calc) => {
                for (i, arg) in calc.args.iter().enumerate() {
                    let CalcArg::Link(child) = arg else {
                        continue;
                    };
                    self.open_json_link(child);
                    if let ParsedLink::Constant(c) = &**child
                        && let Some(Converted::Stored(value)) =
                            constant_load_scalar(c, DbFieldType::Double)
                        && let Some(value) = value.to_f64()
                    {
                        calc.val.set_arg(i, value);
                    }
                }
                if let Some(out) = &calc.out {
                    self.open_json_link(out);
                }
            }
            _ => {}
        }
    }

    /// The lset's [`LinkSet::open_link`](super::LinkSet::open_link) of an
    /// external `link`; `None` when there is no lset to ask. Only a link
    /// naming its scheme has one: a bare CA name is C's `PV_LINK`, which
    /// `dbCaAddLink` takes with no `openLink` (`dbLink.c:128`). The one caller
    /// of `open_link`, so every link is opened once per init and once per put.
    fn open_external_link(
        &self,
        holder: super::LinkHolder<'_>,
        link: &crate::server::record::ParsedLink,
    ) -> Option<super::LinkOpen> {
        let (scheme, lref) = Self::external_link_ref(link.external()?);
        if scheme == "ca" {
            return None;
        }
        super::link_put_queue::find_lset(&self.inner, scheme, |lset| {
            Some(lset.open_link(holder, lref))
        })
    }

    /// The `report` of `link`'s lset ([`LinkSet::report`](super::LinkSet::report));
    /// nothing when no lset takes its scheme.
    pub(crate) fn external_link_report(
        &self,
        link: crate::server::record::ExternalRef<'_>,
        class: Option<crate::types::DbfLinkClass>,
        level: i32,
        indent: usize,
    ) -> Vec<String> {
        let (scheme, lref) = Self::external_link_ref(link);
        super::link_put_queue::find_lset(&self.inner, scheme, |lset| {
            Some(lset.report(lref, class, level, indent))
        })
        .unwrap_or_default()
    }

    /// What an open of `field`'s stored link answered, bound into the record:
    /// no lset is [`RecordInstance::bind_no_lset`](crate::server::record::RecordInstance::bind_no_lset),
    /// an lset gives a forward link its handle.
    fn bind_opened(
        &self,
        instance: &mut crate::server::record::RecordInstance,
        field: &str,
        open: super::LinkOpen,
    ) {
        match open {
            super::LinkOpen::NoLset => instance.bind_no_lset(field),
            super::LinkOpen::Lset => {
                let forward =
                    crate::server::record::declared_field(instance.record.record_type(), field)
                        .and_then(crate::types::link_class_of)
                        == Some(crate::types::DbfLinkClass::FwdLink);
                if let Some(link) = instance.stored_link_shared(field).filter(|_| forward) {
                    self.bind_external_forward(link);
                }
            }
        }
    }

    /// C `dbGetLink`'s failure tail (`dbLink.c:335-339`): nothing for a link
    /// with no lset; otherwise the lset's own message, then `setLinkAlarm`,
    /// whose equal severity leaves that message in place (`recGbl.c:242`).
    #[cold]
    pub(crate) fn get_link_failed(
        &self,
        common: &mut crate::server::record::CommonFields,
        link: &crate::server::record::ParsedLink,
        field: &'static str,
    ) {
        if let Some(alarm) = self.get_link_failure(link, field) {
            crate::server::recgbl::rec_gbl_set_sevr_msg(common, alarm.stat, alarm.sevr, alarm.amsg);
        }
    }

    /// The alarm [`Self::get_link_failed`] raises.
    #[cold]
    fn get_link_failure(
        &self,
        link: &crate::server::record::ParsedLink,
        field: &'static str,
    ) -> Option<LinkAlarm> {
        use crate::server::record::SparseText;
        if crate::server::recgbl::simm::has_no_lset(link) {
            return None;
        }
        let amsg = if self.get_failure_disconn(link) {
            SparseText::link_disconn(field)
        } else {
            SparseText::link_field(field)
        };
        Some(LinkAlarm {
            stat: crate::server::recgbl::alarm_status::LINK_ALARM,
            sevr: crate::server::record::AlarmSeverity::Invalid,
            amsg,
        })
    }

    /// A DB link whose target is not served here reads as the CA link C
    /// makes it: value and raw remote alarm from the ca link set,
    /// identical to the `Ca`/`Pva` arm of
    /// [`Self::read_unresolved_link_value_and_alarm`].
    fn read_db_external(
        &self,
        db: &crate::server::record::DbLink,
    ) -> (Option<EpicsValue>, Option<SourceAlarm>) {
        let external = db.external();
        (
            self.resolve_external_pv(external),
            self.external_link_alarm(external)
                .map(SourceAlarm::external),
        )
    }

    /// [`Self::read_link_value_and_alarm`] for every link that is not a DB
    /// link with a cached target.
    #[cold]
    #[inline(never)]
    fn read_unresolved_link_value_and_alarm(
        &self,
        link: &crate::server::record::ParsedLink,
        reader: Option<&Arc<RecordCell>>,
    ) -> (Option<EpicsValue>, Option<SourceAlarm>) {
        match link {
            crate::server::record::ParsedLink::Db(db) => {
                let target = db.target();
                // C `dbInitLink` locality (`dbLink.c:118-130`): a target
                // record present in this IOC is a DB link read from the
                // local database; a non-local target is a CA link, so its
                // value and raw remote alarm come from the external
                // resolver — identical to the `Ca`/`Pva` arm below. The
                // external name keeps the filter, the local one never has
                // it: `db.pvname()` is C's verbatim `pvname`.
                //
                // The source's alarm is alias-aware (epics-base PR #336) so a
                // link target spelled with an alias still propagates MS/NMS —
                // and it comes off the cell the value was just read from, not
                // a second lookup of the same name.
                let (value, alarm) = match self.read_db_target_by_name(db) {
                    TargetValue::Record(value, rec, alarm) => {
                        (value, alarm.map(|alarm| SourceAlarm::local(rec, alarm)))
                    }
                    TargetValue::NoRecord(value) => (value, None),
                    TargetValue::External => return self.read_db_external(db),
                };
                (value.map(|v| filtered_read(target, v)), alarm)
            }
            // A CONSTANT link delivers nothing at process time; the classifier
            // above turns this `None` into `LinkFetch::NoData` (success), not
            // a failure.
            crate::server::record::ParsedLink::Constant(_) => (None, None),
            // External Pva/Ca link: the value comes from the lset's
            // cached snapshot, the alarm from the lset's accessors.
            //
            // PVA: the `?sevr=` modifier is stripped before epics-base-rs
            // parses the link, so the lset retains and applies the
            // `MS`/`NMS`/`MSI` gate itself — a returned `Some(sev)` is
            // already gated and the caller folds it as `MaximizeStatus`.
            //
            // CA: the `MS`/`NMS`/`MSI`/`MSS` modifier is
            // now carried in the `CaLink`, so the resolver returns the
            // *raw* remote alarm and record processing applies the gate
            // using `link.monitor_switch()`. Either way this fn just
            // reads the raw/gated alarm; the switch pairing happens in
            // `processing.rs`. Without this, a connected external link
            // carrying a remote MINOR/MAJOR severity never folded into
            // the owning record's LINK_ALARM (B2).
            //
            // The name is `external_pv_name`'s, scheme and all: it is also the
            // per-link identity key a structured link resolves by, so two
            // same-PV links keep distinct configs (pvxs per-link
            // `pvaLinkConfig`, ioc/pvalink.h:65).
            crate::server::record::ParsedLink::Pva(_)
            | crate::server::record::ParsedLink::PvaJson(_)
            | crate::server::record::ParsedLink::Ca(_) => {
                let external = link
                    .external()
                    .expect("Ca/Pva/PvaJson link carries a PV name");
                let value = self.resolve_external_pv(external);
                let alarm = self
                    .external_link_alarm(external)
                    .map(SourceAlarm::external);
                if let Some(reader) = reader
                    && let Some(stamp) = self.external_read_stamp(external)
                {
                    stamp_reader(reader, super::processing::ext_time_pair(stamp));
                }
                (value, alarm)
            }
            // A `lnkCalc` link computes its value from its own inputs; its
            // alarm is the `major`/`minor` one its `getValue` raises on the
            // reader, not a source record's. The timestamp half is not
            // here either — C keeps it on the link (`clink->time`) and serves
            // it from the lset (`lnkCalc_getTimestampTag`), so this port
            // answers it from `db_get_time_stamp_tag`, the one owner of
            // link→timestamp.
            crate::server::record::ParsedLink::Calc(calc) => self.read_calc_link(calc, reader),
            // `lnkState`'s lset implements no `getAlarm`, so a state link
            // reads as a value with no alarm to inherit.
            crate::server::record::ParsedLink::State(st) => (Some(self.read_state_link(st)), None),
            // Hardware links carry no generic readable value; `None` links
            // deliver nothing. A removed link has no lset, so `empty_read_fetch`
            // makes its empty read `dbTryGetLink`'s `S_db_noLSET`.
            crate::server::record::ParsedLink::Hw(_)
            | crate::server::record::ParsedLink::None
            | crate::server::record::ParsedLink::Removed(_) => (None, None),
        }
    }

    /// C `dbGetTimeStampTag` on an external link: its lset's
    /// [`LinkSet::time_stamp`](super::LinkSet::time_stamp).
    pub(crate) fn external_link_time(
        &self,
        link: crate::server::record::ExternalRef<'_>,
    ) -> Option<(i64, i32, u64)> {
        let (scheme, lref) = Self::external_link_ref(link);
        super::link_put_queue::find_lset(&self.inner, scheme, |lset| lset.time_stamp(lref))
    }

    /// The stamp an external link's `getValue` writes into its reader:
    /// [`LinkSet::read_time_stamp`](super::LinkSet::read_time_stamp).
    fn external_read_stamp(
        &self,
        link: crate::server::record::ExternalRef<'_>,
    ) -> Option<(i64, i32, u64)> {
        let (scheme, lref) = Self::external_link_ref(link);
        super::link_put_queue::find_lset(&self.inner, scheme, |lset| lset.read_time_stamp(lref))
    }

    /// Build a [`LinkAlarm`] from the registered lset's alarm
    /// accessors for an external (`pva://` / `ca://`) link, or `None`
    /// when no lset is registered or the lset reports no alarm.
    ///
    /// The lset's `alarm_severity` is the gated severity (see
    /// [`crate::server::database::LinkSet::alarm_severity`]); when it
    /// is `Some`, the `stat` is `LINK_ALARM` and the message comes
    /// from `alarm_message`.
    fn external_link_alarm(
        &self,
        link: crate::server::record::ExternalRef<'_>,
    ) -> Option<LinkAlarm> {
        let (scheme, lref) = Self::external_link_ref(link);
        super::link_put_queue::find_lset(&self.inner, scheme, |lset| {
            let sev = lset.alarm_severity(lref)?;
            Some(LinkAlarm {
                // remote STAT for MSS, else LINK_ALARM.
                stat: lset
                    .alarm_status(lref)
                    .map(|s| s as u16)
                    .unwrap_or(crate::server::recgbl::alarm_status::LINK_ALARM),
                sevr: crate::server::record::AlarmSeverity::from_u16(sev as u16),
                amsg: lset
                    .alarm_message(lref)
                    .map(crate::server::record::SparseText::from)
                    .unwrap_or_default(),
            })
        })
    }

    /// Ungated remote alarm snapshot for an external (`pva://` /
    /// `ca://`) link — the DB-link inspection counterpart of
    /// `Self::external_link_alarm`.
    ///
    /// Where `external_link_alarm` returns the **gated** maximize-severity
    /// contribution folded into the owning record's `LINK_ALARM` (pvxs
    /// `pvaGetValue` applying the `MS`/`NMS`/`MSI` gate,
    /// `pvxs/ioc/pvalink_lset.cpp:424-431`), this returns the **ungated** remote
    /// `(severity, status, message)` snapshot pvxs exposes through
    /// `dbGetAlarm` / `dbGetAlarmMsg` (`pvaGetAlarmMsg`,
    /// `pvxs/ioc/pvalink_lset.cpp:542-569`; `pvaGetAlarm` `:571-575` is its
    /// no-message-buffer wrapper). A default `NMS` link reports its
    /// remote severity here even though it leaves the owning record
    /// unraised.
    ///
    /// `None` when no lset is registered for the scheme, the link is not
    /// connected, or the lset does not track remote alarms. Scheme
    /// dispatch mirrors `Self::external_link_alarm`.
    pub fn external_link_alarm_snapshot(
        &self,
        link: crate::server::record::ExternalRef<'_>,
    ) -> Option<crate::server::database::RemoteAlarm> {
        let (scheme, lref) = Self::external_link_ref(link);
        super::link_put_queue::find_lset(&self.inner, scheme, |lset| lset.remote_alarm(lref))
    }

    /// Remote display / control / valueAlarm metadata for an external
    /// (`pva://` / `ca://`) link, resolved through the registered
    /// lset's [`crate::server::database::LinkSet::link_metadata`] hook.
    ///
    /// This is the DB-link-API entry point that exposes the linked PV
    /// metadata pvxs's pvalink lset surfaces through its
    /// `pvaGetDBFtype` / `pvaGetElements` / `pvaGetControlLimits` /
    /// `pvaGetGraphicLimits` / `pvaGetAlarmLimits` / `pvaGetPrecision`
    /// / `pvaGetUnits` getters
    /// (`pvxs/ioc/pvalink_lset.cpp:706-732`). Scheme dispatch mirrors
    /// `Self::external_link_alarm`: an explicit `pva://` / `ca://`
    /// prefix selects the lset directly, a bare name tries every
    /// registered lset until one reports metadata.
    ///
    /// `None` when no lset is registered for the scheme or the lset
    /// has no cached value for the link (not yet connected).
    pub fn external_link_metadata(
        &self,
        link: crate::server::record::ExternalRef<'_>,
    ) -> Option<crate::server::database::LinkMetadata> {
        let (scheme, lref) = Self::external_link_ref(link);
        super::link_put_queue::find_lset(&self.inner, scheme, |lset| lset.link_metadata(lref))
    }

    /// [`LinkSet::get_cached_enum_string`](super::LinkSet::get_cached_enum_string)
    /// of an external link.
    fn external_enum_string(
        &self,
        link: crate::server::record::ExternalRef<'_>,
    ) -> super::EnumString {
        let (scheme, lref) = Self::external_link_ref(link);
        super::link_put_queue::find_lset(&self.inner, scheme, |lset| {
            Some(lset.get_cached_enum_string(lref))
        })
        .unwrap_or(super::EnumString::NotServed)
    }

    /// C `dbGetControlLimits` / `dbGetGraphicLimits` / `dbGetAlarmLimits` /
    /// `dbGetPrecision` / `dbGetUnits` (`dbLink.c:344-393`) — the five
    /// metadata slots a record fetches THROUGH one of its links.
    ///
    /// The single owner of "what does this link say about display/control/
    /// alarm limits, units and precision". Record support (the routing
    /// layer) calls this from its `get_graphic_double` / `get_control_double`
    /// / `get_alarm_double` / `get_units` / `get_precision` implementations
    /// for the link-backed fields — calc/calcout/sub/aSub INPA..INPU,
    /// aSub OUTA.., seq DOn (`calcRecord.c:223-230`, `aSubRecord.c:356-369`,
    /// `subRecord.c:260-266`, `seqRecord.c:332-336`).
    ///
    /// # Return contract — this is C's status, not a convenience `Option`
    ///
    /// Each of C's five `dbGet*` entry points writes the caller's buffer
    /// ONLY on a zero return; every caller listed above ignores the status
    /// and keeps whatever the buffer already held. The two levels of
    /// `Option` here encode exactly that:
    ///
    /// * `None` — C returned non-zero (`S_db_noLSET` / `S_dbLib_badLink`).
    ///   The caller MUST leave its buffer untouched.
    /// * `Some(meta)` — C returned 0. Each `Some` field is a value C wrote;
    ///   a `None` field is a slot this link cannot report.
    ///
    /// # Per-link-class behaviour, from the C lset tables
    ///
    /// * **Constant link** — `None`, always. `dbConst_lset`
    ///   (`dbConstLink.c:234-248`) leaves all five slots `NULL`, so the
    ///   `!plset->getGraphicLimits` test in `dbGetGraphicLimits`
    ///   (`dbLink.c:358-359`) short-circuits to `S_db_noLSET` and NOTHING is
    ///   written. This is the case for the oracle's `field(INPA,"5")`
    ///   records: C's answer is *not* a propagated limit and *not* a
    ///   DBF-type-range default — the record's buffer keeps the seed it held
    ///   before the fetch (see the routing contract below). Measured: a
    ///   `calc` `field(INPA,"5")` serves display limits `0/0`.
    /// * **DB link** — `Some(meta)` from the target field, via
    ///   `Self::db_link_metadata`.
    /// * **CA/PVA link** — delegated to the registered lset
    ///   ([`Self::external_link_metadata`]); C installs `dbCa`/`pvalink`'s
    ///   own lset for these, not `dbDb_lset`.
    /// * **Everything else** (unset link, hardware link) — `None`. C leaves
    ///   `plink->lset` NULL for these, so `dbLink.c:358` returns
    ///   `S_db_noLSET`.
    ///
    /// `visited` is the caller's chain state and carries C's
    /// `DBLINK_FLAG_VISITED` recursion guard — see
    /// `Self::db_link_metadata`.
    ///
    /// # Contract for the routing layer
    ///
    /// This method reports what the LINK says. It does not know what the
    /// calling record should do with a `None`, because C's answer to that is
    /// per-slot and not uniform. Record support must seed its buffer, call
    /// this, and overwrite only on `Some` — the seed IS the answer whenever
    /// the fetch reports nothing. C's seeds, and the four slots that route
    /// through a link at all:
    ///
    /// | rset slot | seed before the fetch | C site |
    /// |---|---|---|
    /// | `get_graphic_double` | `(0.0, 0.0)` | `dbAccess.c:216` |
    /// | `get_alarm_double` | `NaN×4` | `dbAccess.c:290` |
    /// | `get_units` | `""` (NOT the record's `EGU`) | `dbAccess.c:378` |
    /// | `get_precision` | the record's own `PREC` | `calcRecord.c:191` |
    /// | `get_control_double` | **never fetched — see below** | |
    ///
    /// Two of those seeds are counter-intuitive and are measured facts, not
    /// deductions (softIoc + `caget -d DBR_CTRL_DOUBLE`, EPICS 7 base): on a
    /// `calc` with `field(INPA,"5") field(PREC,"7") field(EGU,"volts")`, C
    /// serves `INPA` with **precision 7** (the record's own `PREC` survives,
    /// because `get_precision` seeds `*pprecision = prec->prec` *before* the
    /// fetch and overwrites only on a zero status) but **units `""`** (the
    /// `strncpy(units, prec->egu)` fallback sits in the `else` arm that a
    /// link-backed field never takes — `calcRecord.c:172-181`).
    ///
    /// **Control limits must NOT be routed through a link.** `dbDbLink.c:414`
    /// really does install `dbDbGetControlLimits`, and `dbGetControlLimits` is
    /// public API (`dbLink.h:436`) — so this method reports it — but NOTHING
    /// in EPICS base calls it. Every record instead sends its link-backed
    /// fields to `recGblGetControlDouble` (`calcRecord.c:251`,
    /// `subRecord.c`, `calcoutRecord.c`, `aSubRecord.c:376`), i.e. to
    /// `getMaxRangeValues` — which is why C serves a `calc` `INPA` with
    /// `±1e300` control limits (measured) regardless of what the link points
    /// at. Routing this method's `control_limits` into a record's
    /// `get_control_double` would be a defect.
    pub fn link_metadata(
        &self,
        link: &crate::server::record::ParsedLink,
        visited: &mut HashSet<RecordName>,
    ) -> Option<crate::server::database::LinkMetadata> {
        self.link_metadata_through(link, visited, super::ReadPolicy::Wait)
    }

    /// [`Self::link_metadata`] taking each target as `policy` says.
    pub(crate) fn link_metadata_through(
        &self,
        link: &crate::server::record::ParsedLink,
        visited: &mut HashSet<RecordName>,
        policy: super::ReadPolicy<'_>,
    ) -> Option<crate::server::database::LinkMetadata> {
        use crate::server::record::ParsedLink;
        match link {
            ParsedLink::Db(db) => self.db_link_metadata(db, visited, policy),
            ParsedLink::Ca(_) | ParsedLink::Pva(_) | ParsedLink::PvaJson(_) => {
                let external = link
                    .external()
                    .expect("Ca/Pva/PvaJson link carries a PV name");
                self.external_link_metadata(external)
            }
            // Constant / unset / hardware / lnkCalc: no metadata lset slots.
            _ => None,
        }
    }

    /// C `dbDbGetControlLimits` / `dbDbGetGraphicLimits` /
    /// `dbDbGetAlarmLimits` / `dbDbGetPrecision` / `dbDbGetUnits`
    /// (`dbDbLink.c:263-345`) — the `dbDb_lset` metadata slots
    /// (`dbDbLink.c:414-415`).
    ///
    /// Each C slot is one `dbDbGetOptionLoopSafe(plink, DBR_DOUBLE, &buf,
    /// DBR_<OPTION>)` (`dbDbLink.c:239-261`), i.e. a `dbGet` on the target's
    /// `dbAddr` asking for one option. This port issues the equivalent single
    /// [`snapshot_for_field`] — the port's `dbGet`-with-options — and reads
    /// all five out of it. One fetch instead of five is observationally
    /// identical: `dbGet` computes each option independently from the target
    /// record's own rset slots.
    ///
    /// # The recursion guard is load-bearing
    ///
    /// C flags the link `DBLINK_FLAG_VISITED` across the inner `dbGet` and
    /// clears it after (`dbDbLink.c:248-258`), because — in its own words
    /// (`dbDbLink.c:236-238`) — "Some records get options (precsision,
    /// units, ...) for some fields from an input link. We need to catch the
    /// case that this link points back to the same field or we will end in
    /// an infinite recursion." A re-entered link leaves `status` at its
    /// `S_dbLib_badLink` initialiser (`dbDbLink.c:246`) and writes nothing.
    ///
    /// The port cannot hang a flag on the link: [`ParsedLink`] is a cloned
    /// value, not C's stable `struct link *`. The guard therefore rides in
    /// the caller's `visited` set, keyed by link TARGET (`record.field`) —
    /// the same substitution `process_passive_db_source` makes for C's PACT
    /// guard, and it terminates the identical cycles: an `A.INPA -> B.VAL`,
    /// `B.INPA -> A.VAL` pair blocks on the second visit to `B.VAL`.
    ///
    /// # Fallback chain when the target field has no support
    ///
    /// `dbGet` does NOT fail when the target's rset lacks a slot — it fills
    /// a default, turns the option bit off, and still returns 0, so the
    /// `dbDbGet*` wrapper reads that default out and returns 0 (writing it
    /// to the record). Transcribed per option:
    ///
    /// | slot | no support on target | C site |
    /// |---|---|---|
    /// | graphic limits | `(0.0, 0.0)` — `memset` | `dbAccess.c:241-242` |
    /// | control limits | `(0.0, 0.0)` — `memset` | `dbAccess.c:281-283` |
    /// | alarm limits | `NaN×4` — pre-filled initialiser, assigned unconditionally | `dbAccess.c:294,317-329` |
    /// | precision | `0` — `memset`, also when the field is not FLOAT/DOUBLE | `dbAccess.c:387-395` |
    /// | units | `""` — `memset` | `dbAccess.c:377-386` |
    ///
    /// Note graphic/control zero but alarm is NaN: `get_alarm` seeds
    /// `{epicsNAN,...}` and assigns the buffer whether or not the slot ran,
    /// whereas `get_graphics`/`get_control` `memset` the buffer in their
    /// no-data arm. The port's [`Snapshot`] accessors already return `None`
    /// for exactly "record type has no such rset slot", so each default is a
    /// single `unwrap_or` below.
    ///
    /// This is why the link layer never reaches `getMaxRangeValues`: C's
    /// DBF-type-range default is produced by the TARGET record's own
    /// `get_graphic_double` calling `recGblGetGraphicDouble`
    /// (`recGbl.c:146-153`), which is inside `snapshot_for_field` — it comes
    /// back through the "has support" arm, already applied.
    ///
    /// [`snapshot_for_field`]: crate::server::record::RecordInstance::snapshot_for_field
    /// [`Snapshot`]: crate::server::snapshot::Snapshot
    /// [`ParsedLink`]: crate::server::record::ParsedLink
    fn db_link_metadata(
        &self,
        db: &crate::server::record::DbLink,
        visited: &mut HashSet<RecordName>,
        policy: super::ReadPolicy<'_>,
    ) -> Option<crate::server::database::LinkMetadata> {
        let key = db.pvname().clone();
        // C `if (!(mutable_plink->flags & DBLINK_FLAG_VISITED))`
        // (`dbDbLink.c:253`): a re-entered link returns the `S_dbLib_badLink`
        // initialiser without touching the buffer.
        if !visited.insert(key.clone()) {
            return None;
        }
        let meta = self.db_target_metadata(db, visited, policy);
        // C `mutable_plink->flags &= ~DBLINK_FLAG_VISITED` (`dbDbLink.c:257`)
        // — the guard spans only the inner fetch, so a diamond (two distinct
        // links onto one target) still reports metadata on both.
        visited.remove(&key);
        meta
    }

    /// The target-side half of [`Self::db_link_metadata`], with C
    /// `dbInitLink`'s locality dispatch (`dbLink.c:118-130`): a target that
    /// is not in this IOC never gets `dbDb_lset` at all — it is made a CA
    /// link, so its metadata comes from the `dbCa` lset. Mirrors the same
    /// split [`Self::read_link_value_and_alarm`] makes for the value path.
    fn db_target_metadata(
        &self,
        db: &crate::server::record::DbLink,
        visited: &mut HashSet<RecordName>,
        policy: super::ReadPolicy<'_>,
    ) -> Option<crate::server::database::LinkMetadata> {
        let target = db.target();
        let record = match self.db_link_target(db) {
            LinkTarget::Local(rec) => rec,
            // Local but not a record: no field to carry metadata, and C would
            // not have made this a CA link either.
            LinkTarget::LocalNotRecord => return None,
            LinkTarget::External => return self.external_link_metadata(db.external()),
        };
        // C `dbGet(paddr, DBR_DOUBLE, &buffer, &option, ...)` under the
        // target's lock (`dbDbLink.c:252-256` between `dbScanLock` /
        // `dbScanUnlock`). A field the target does not have is C's
        // `dbNameToAddr` failure at link-init time, i.e. no lset — `None`.
        // `PvDatabase::channel_snapshot_for_field_guarded`, not the
        // `RecordInstance` method: the target's own field may itself be
        // link-backed (a `calc` whose `INPA` names another `calc`'s `A`), and
        // only the database can take the second hop — it is the half that
        // holds no lock while it resolves. `visited` is the caller's, so C's
        // one `DBLINK_FLAG_VISITED` guard spans the whole chain.
        //
        // No `$` view: a link target is `REC.FLD` out of a link string, which
        // C resolves with `dbNameToAddr` (`dbDbLink.c:60-70`). Only
        // `dbChannelCreate` parses a `$`, and no link ever goes through it.
        let snapshot = self.channel_snapshot_for_field_guarded(
            &record,
            &target.field,
            false,
            visited,
            policy,
        )?;
        Some(crate::server::database::LinkMetadata {
            // `dbDbGetDBFtype` = `dbChannelFinalFieldType` (`dbDbLink.c:151-155`)
            // and `dbDbGetElements` = `dbChannelFinalElements`
            // (`dbDbLink.c:157-162`). A local DB link is always connected
            // (`dbDbIsConnected` returns TRUE unconditionally,
            // `dbDbLink.c:146-149`), so these are never `None` here.
            dbf_type: Some(field_type_to_link_dbf(snapshot.value.db_field_type())),
            element_count: Some(snapshot.value.count() as i64),
            graphic_limits: Some(snapshot.graphic_limits().unwrap_or((0.0, 0.0))),
            control_limits: Some(snapshot.control_limits().unwrap_or((0.0, 0.0))),
            alarm_limits: Some(snapshot.alarm_limits().unwrap_or((
                f64::NAN,
                f64::NAN,
                f64::NAN,
                f64::NAN,
            ))),
            precision: Some(snapshot.precision().unwrap_or(0)),
            units: Some(
                snapshot
                    .units()
                    .map(|u| u.as_str_lossy().into_owned())
                    .unwrap_or_default(),
            ),
            // `display.description` has no C lset slot — it is a pvxs-only
            // pvalink extra (`LinkMetadata::description`).
            description: None,
        })
    }

    /// C `dbGetLink` PP rule: if a DB input link is `ProcessPassive`
    /// and its source record is `Passive`-scanned, process the source
    /// record before its value is read so the reader sees a freshly
    /// computed value. No-op for non-PP links or non-passive sources.
    ///
    /// Shared by `read_link_value_soft` (single-INP path) and the
    /// multi-input fetch loop (`INPA..INPL` for calc/sel/sub/aSub) so
    /// both paths get the identical C-correct PP-processing behavior.
    ///
    /// The caller's `visited` set is threaded through into the source's
    /// processing cycle — NOT a fresh set. This is required for the
    /// cycle guard to span the PP hop: in C,
    /// `calcRecord.c::process` sets `prec->pact = TRUE` *before*
    /// `fetch_values()` (calcRecord.c:119-120), so when a PP input link
    /// re-enters `dbProcess` on a record already mid-fetch, the
    /// `if (precord->pact) goto all_done;` guard (dbAccess.c:537-557)
    /// terminates the cycle after one bounce. The Rust port sets its
    /// PACT `AtomicBool` only on `AsyncPending` *after* `record.process()`
    /// returns, so it cannot catch a record mid-link-fetch. Threading
    /// the caller's `visited` set makes the existing `visited.insert`
    /// cycle guard (`process_record_with_links_inner`, processing.rs)
    /// fire instead — an A↔B `PP` cycle bails when the second hop tries
    /// to re-insert a name already on the chain. The FLNK path threads
    /// `visited` the same way (processing.rs FLNK dispatch).
    ///
    /// The policy test is the caller's, so a link without PP costs no call.
    #[inline(always)]
    pub(crate) fn process_passive_db_source(
        &self,
        src: Option<&Arc<RecordCell>>,
        db: &crate::server::record::DbLink,
        visited: &mut ProcStack,
    ) -> DbResult {
        if db.policy == crate::server::record::LinkProcessPolicy::ProcessPassive {
            return self.process_pp_db_source(src, db, visited);
        }
        Ok(())
    }

    /// C `dbDbGetValue`'s `dbScanPassive(precord, …)` (`dbDbLink.c:175`):
    /// `processTarget` with the reader as `psrc`, so the source takes the
    /// reader's PUTF (or RPRO when busy) and joins its put-notify, as a
    /// forward link's target does.
    #[inline(never)]
    fn process_pp_db_source(
        &self,
        src: Option<&Arc<RecordCell>>,
        db: &crate::server::record::DbLink,
        visited: &mut ProcStack,
    ) -> DbResult {
        let Some(at) = self.db_link_addr(db) else {
            return Ok(());
        };
        let (putf, notify) = src.map_or((false, None), |src| {
            let reader = src.read();
            (reader.common.putf, reader.notify.clone())
        });
        let src = TargetSrc {
            rec: src,
            putf,
            notify: notify.as_ref(),
        };
        self.process_target_at(src, &at.rec, ProcessTargetGate::ScanPassive, None, visited)
    }

    /// Read a value from a parsed link for INP (only reads DB links when soft channel).
    ///
    /// `visited` is the caller's processing-chain state — a PP
    /// input link's source is processed *within* that same chain so the
    /// `visited` cycle guard spans the PP hop (see
    /// `Self::process_passive_db_source`).
    pub fn read_link_value_soft(
        &self,
        link: &crate::server::record::ParsedLink,
        is_soft: bool,
        visited: &mut ProcStack,
    ) -> Option<EpicsValue> {
        self.read_link_value_soft_with_alarm(None, link, is_soft, false, visited)
            .0
    }

    /// [`Self::read_link_value_soft`] and the source's alarm off the SAME
    /// read. The arms decide only WHETHER the link reads this cycle; the read
    /// is [`Self::read_link_value_and_alarm_at_source`]'s, with the source's
    /// `DBR_STRING` part when `render` asks for it.
    pub(crate) fn read_link_value_soft_with_alarm(
        &self,
        src: Option<&Arc<RecordCell>>,
        link: &crate::server::record::ParsedLink,
        is_soft: bool,
        render: bool,
        visited: &mut ProcStack,
    ) -> RenderedRead {
        match link {
            // A CONSTANT input link delivers NOTHING at process time. C
            // `dbConstGetValue` (`dbConstLink.c:219-225`) returns status 0
            // having written nothing to the buffer, so `read_ai`'s
            // `dbGetLink(&prec->inp, ...)` leaves VAL exactly as it was; the
            // array/soft-callback dev supports state the same rule outright
            // (`devWfSoft.c::read_wf` and `devAaiSoft.c::read_aai`:
            // `if (dbLinkIsConstant(pinp)) return 0;`). The constant reaches
            // the record ONCE, at init, through the
            // `recGblInitConstantLink`/`dbLoadLinkArray` owner
            // (`rec_gbl_init_constant_links`).
            //
            // Re-delivering it every cycle — as this arm did — made a constant
            // INP overwrite the record's VAL on every process, so a client
            // caput to a `field(INP, "5")` ai (or the data a client pushed
            // into an aai/waveform whose INP is an unset/constant link, which
            // is how EVERY device-fed and client-fed array record is
            // configured) was clobbered on the next scan.
            crate::server::record::ParsedLink::Constant(_) => (None, None, None),
            crate::server::record::ParsedLink::Db(_) if is_soft => {
                self.read_link_value_and_alarm_at_source(src, link, render, visited)
            }
            crate::server::record::ParsedLink::Ca(_)
            | crate::server::record::ParsedLink::Pva(_)
            | crate::server::record::ParsedLink::PvaJson(_)
                if is_soft =>
            {
                self.read_link_value_and_alarm_at_source(src, link, render, visited)
            }
            // lnkCalc evaluates regardless of `is_soft` — the input
            // PVs may themselves be local DB targets (which need the
            // soft path) or remote CA/PVA, but the calc evaluation
            // is uniform either way.
            crate::server::record::ParsedLink::Calc(_) => {
                self.read_link_value_and_alarm_at_source(src, link, render, visited)
            }
            // A state link answers whatever the DTYP, for the same reason a
            // calc link does: `lnkState`'s own lset implements `getValue`
            // (`lnkState.c:139-151`), so there is no device support to route
            // around and no target record to process first.
            crate::server::record::ParsedLink::State(_) => {
                self.read_link_value_and_alarm_at_source(src, link, render, visited)
            }
            _ => (None, None, None),
        }
    }

    /// C `processTarget` (dbDbLink.c:474-528) — and the gate that is the ONLY
    /// way to reach it. The single owner of the link-side `PUTF`/`RPRO`
    /// transition.
    ///
    /// **Invariant:** `PUTF`/`RPRO` on a link target are written only by
    /// `processTarget`, and `processTarget` is reachable only for a PASSIVE
    /// target or a `.PROC` write. C reaches it through exactly two gates, both
    /// of which return BEFORE it otherwise — see [`ProcessTargetGate`].
    ///
    /// The port had five clones of the body, each applying the Passive test to
    /// the *process* call only and mutating PUTF/RPRO above it. So an FLNK to a
    /// busy periodic record got `RPRO = 1`, and its async completion fired an
    /// extra unscheduled cycle — an extra device write and an extra FLNK chain
    /// (R18-93). The gate now lives inside the owner, where it cannot be
    /// forgotten.
    ///
    /// The body is C's, in order:
    ///
    /// * target not PACT — normal propagation, `target.putf = src_putf`, and
    ///   the put-notify join (C `dbNotifyAdd`, dbDbLink.c:460);
    /// * target PACT, `src_putf`, and the target is not already on this process
    ///   chain (C `claim_dst`) — `target.rpro = true`, `target.putf = false`, so
    ///   the in-flight cycle reprocesses on completion and attributes the put to
    ///   the originator;
    /// * otherwise nothing: the target is being processed recursively by us, or
    ///   this was not a `dbPutField`.
    ///
    /// An active target still goes to `dbProcess`: C calls it unconditionally
    /// (`dbDbLink.c:512` at R7.0.10) and lets `dbProcess`'s own already-active
    /// arm refuse it, which is what counts the refusal in LCNT
    /// (`dbAccess.c:536-556`). Gating the call here would skip that count.
    ///
    /// The target is the `dbAddr`'s `precord` C's `processTarget` is handed,
    /// already resolved.
    ///
    /// `held` is the caller's frame set, which a target merged into it is
    /// written under without taking it again.
    pub(crate) fn process_target_at(
        &self,
        src: TargetSrc<'_>,
        target_rec: &Arc<crate::server::record::RecordCell>,
        gate: ProcessTargetGate,
        held: Option<&super::SetGuard>,
        visited: &mut ProcStack,
    ) -> DbResult {
        if !self.admit_target(src, target_rec, gate, held, visited) {
            return Ok(());
        }
        // Recursive target processing within one chain — the gate is already
        // held by the foreign entry record. Unconditional, as C's
        // `dbProcess(pdst)` is: an active target is refused inside
        // `process_record_with_links_body`'s PACT arm, the single owner of
        // that decision, and the refusal is counted there. Its status is
        // `processTarget`'s return (`dbDbLink.c:511-528`).
        let _pact = src.rec.map(|src| src.hold_chain_pact());
        self.process_record_with_links_recursive_at(target_rec.name(), target_rec, visited)
            .map_err(|e| DbStatus::of_error(&e))
    }

    /// [`Self::process_target_at`] up to `dbProcess`: whether the target
    /// passes the gate, with its PUTF/RPRO and put-notify join written.
    #[inline(always)]
    pub(super) fn admit_target(
        &self,
        src: TargetSrc<'_>,
        target_rec: &Arc<crate::server::record::RecordCell>,
        gate: ProcessTargetGate,
        held: Option<&super::SetGuard>,
        visited: &ProcStack,
    ) -> bool {
        let TargetSrc {
            rec: src_rec,
            putf: src_putf,
            notify: src_notify,
        } = src;
        let mut tg = target_rec.write_under(held);
        // A forward bound before the target was removed still reaches its
        // cell (the record table keeps it); a removed record runs nothing.
        if tg.is_destroyed() {
            return false;
        }
        // The gate. A target that does not pass it never reaches
        // `processTarget`, so it gets no PUTF, no RPRO, and no put-notify
        // join — the join in particular would `enter` the wait-set without
        // ever `leave`ing it.
        if !gate.admits(tg.common.scan) {
            return false;
        }
        {
            // C `dbDbLink.c:439`: the source, when its set traces.
            let traced = src_rec
                .filter(|src| super::db_access_debug_putf() != 0 && src.lock_record().tracing());
            // C's `pdst->pact` is both halves: a cycle on this chain (C's
            // `processTarget` marker, `claim_dst` false) or an async wait.
            let on_chain = visited.holds(target_rec);
            if !on_chain && !tg.is_processing() {
                if let Some(src) = traced {
                    super::processing::trace_dbprocess(format_args!(
                        "'{}' -> '{}' with PUTF={}",
                        src.name(),
                        tg.name,
                        u8::from(src_putf),
                    ));
                }
                tg.common.putf = src_putf;
                tg.join_put_notify(src_notify);
            } else if src_putf && !on_chain {
                if let Some(src) = traced {
                    super::processing::trace_dbprocess(format_args!(
                        "'{}' -> Active '{}', setting RPRO=1",
                        src.name(),
                        tg.name,
                    ));
                }
                tg.common.rpro = 1;
                tg.common.putf = false;
            } else if let Some(src) = traced {
                super::processing::trace_dbprocess(format_args!(
                    "'{}' -> Active '{}', done",
                    src.name(),
                    tg.name,
                ));
            }
        }
        true
    }

    /// C `dbDbPutValue` (`dbDbLink.c:372-392`) for a caller with no frame —
    /// a test's direct write. Returns `true` when the put failed.
    #[cfg(test)]
    pub(crate) fn write_db_link_value(
        &self,
        link: &crate::server::record::DbLink,
        value: EpicsValue,
        src: OutLinkSrc<'_>,
        visited: &mut ProcStack,
    ) -> bool {
        let link = Arc::new(crate::server::record::ParsedLink::Db(Arc::new(
            link.clone(),
        )));
        let mut staged = StageSlot::default();
        let mut out = LinkWriteOut {
            staged: &mut staged.0,
            visited,
        };
        let write = self.write_db_link_value_in(
            None,
            link,
            super::field_io::PutSrc::Owned(value),
            &src,
            &mut out,
        );
        self.finish_db_link_write(write, out.staged, None, src, out.visited)
            .is_err()
    }

    /// [`Self::write_db_link_value_in`] for a DB link whose target is not
    /// served here: the `dbCaPutLink` of the CA link C made of it.
    #[cold]
    #[inline(never)]
    fn write_db_external(
        &self,
        frame: Option<InFrame<'_>>,
        db: &crate::server::record::DbLink,
        src: super::field_io::PutSrc<'_>,
        src_out: &OutLinkSrc<'_>,
    ) -> DbLinkWrite {
        let external = db.external();
        let failed = match self.external_put_value(external, src) {
            None => false,
            Some(value) => {
                match self.write_external_pv(
                    src_out.link_id(frame),
                    external,
                    value,
                    src_out.external_done(),
                ) {
                    Ok(()) => false,
                    Err(refusal) => {
                        refusal.report(external.name);
                        true
                    }
                }
            }
        };
        DbLinkWrite::untargeted(failed)
    }

    /// The `dbPut` of C `dbDbPutValue` (`dbDbLink.c:372-380`) through `link`,
    /// a DB link, and the one place its address lives: lent under `frame`'s
    /// set (the writer's), or the owned answer on a route that holds none. The address and the handle end here, before anything that can
    /// park and so let the owner's set go (a special's link write, PP): a
    /// link swapped during a park is freed by the swapper, never by the
    /// frame. What the rest of `dbDbPutValue` needs comes back owned.
    ///
    /// A target record not present in this IOC is a CA link (`dbInitLink`
    /// locality, `dbLink.c:118-130`): its write goes to the external put
    /// path (`dbCaPutLink`), which performs none of the inherit / PUTF /
    /// `processTarget` body, so it comes back with no target.
    pub(crate) fn write_db_link_value_in(
        &self,
        frame: Option<InFrame<'_>>,
        link: Arc<crate::server::record::ParsedLink>,
        src: super::field_io::PutSrc<'_>,
        src_out: &OutLinkSrc<'_>,
        out: &mut LinkWriteOut<'_>,
    ) -> DbLinkWrite {
        let held = frame.map(|f| f.set());
        use crate::server::record::DbLinkAnswer;
        use crate::server::record::record_instance::LinkTargetResolver;
        let crate::server::record::ParsedLink::Db(db) = &*link else {
            unreachable!("write_db_link_value_in takes a DB link");
        };
        let target = db.target();
        let lent = held.and_then(|held| {
            db.lend_out_addr(held, self.name_revision(), || self.resolve_db_link(db))
        });
        let owned;
        let (at, shadow) = match lent {
            Some(at) => (Some(at), None),
            None => match self.db_link_answer(db) {
                DbLinkAnswer::Local(at) => {
                    owned = at;
                    (Some(&owned), None)
                }
                DbLinkAnswer::Shadowed(pv) => (None, Some(pv)),
                DbLinkAnswer::Absent => (None, None),
                DbLinkAnswer::External => {
                    return self.write_db_external(frame, db, src, src_out);
                }
            },
        };
        if let Some(at) = at {
            let to = super::field_io::PutTarget {
                name: &at.name,
                rec: &at.rec,
                field: &target.field,
                at: &at.put,
            };
            let acts = TargetActs {
                gate: ProcessTargetGate::of_put(db),
                switch: db.monitor_switch,
            };
            return self.write_local_target(held, to, acts, src, out);
        }
        let mut tail = super::field_io::PutTail::default();
        let failed = match shadow {
            // `put_pv_body`'s simple-PV arm, with the PV already settled.
            Some(pv) => {
                pv.set(src.into_value());
                false
            }
            None => {
                let mut chain = super::field_io::PutChain::root(&mut *out.visited);
                let put = self.put_pv_body(&local_pv_name(target), src.into_value(), &mut chain);
                tail = chain.tail;
                put.is_err()
            }
        };
        if tail.has_report() {
            self.emit_cycle_report(target.record.as_bytes(), &target.field, tail);
        }
        // No local target: no inherit and no `processTarget` to run.
        DbLinkWrite::untargeted(failed)
    }

    /// [`Self::write_db_link_value_in`] at a local target: C `dbDbPutValue`'s
    /// `dbPut` through `to`, the gate and MS its link settled.
    ///
    /// No put-notify restart is armed by this write, whether or not PP drives
    /// a cycle: C's owner is the cycle tail (`recGblFwdLink` →
    /// `dbNotifyCompletion`), so a target whose park this releases replays on
    /// its next cycle. A refusal's report prints here, under the cycle's hold
    /// as in C, and `special()`'s after-gate work is discarded.
    #[inline(always)]
    fn write_local_target(
        &self,
        held: Option<&super::SetGuard>,
        to: super::field_io::PutTarget<'_>,
        TargetActs { gate, switch }: TargetActs,
        src: super::field_io::PutSrc<'_>,
        out: &mut LinkWriteOut<'_>,
    ) -> DbLinkWrite {
        // C `dbPutLinkLS` (`dbLink.c:527-538`): only a `DBF_CHAR`/`DBF_UCHAR`
        // field takes the characters; any other takes one `DBR_STRING`, lent
        // from the source's buffer up to its NUL.
        let ls_str = match src.long_string() {
            Some(chars) if !takes_chars(held, to.rec, to.at, to.field) => {
                Some(super::char_view::long_string_field_bytes(chars))
            }
            _ => None,
        };
        let mut tail = super::field_io::PutTail::default();
        let put = self.db_put_at(
            held,
            to,
            match ls_str {
                Some(bytes) => super::field_io::PutReq::Str(bytes),
                None => src.into(),
            },
            0,
            super::field_io::PutVia::DbPut,
            &mut tail,
        );
        // The report prints after the special's link writes, as `dbPut`'s
        // own does; with none to run it prints here.
        let failed = put.is_err();
        if let Ok(Some((actions, rest))) = put {
            let rest = rest.detach();
            let report = tail.has_report().then(|| {
                let field = rest.field();
                (tail, field)
            });
            *out.staged = Some(LinkStage {
                stage: (actions, rest),
                report,
            });
        } else if tail.has_report() {
            self.emit_cycle_report(to.name, to.field, tail);
        }
        // The owned target, a refcount bump, only for what still acts on it.
        let needed = out.staged.is_some()
            || switch != crate::server::record::MonitorSwitch::NoMaximize
            || (gate.is_some() && !failed);
        DbLinkWrite {
            target: needed.then(|| Arc::clone(to.rec)),
            switch,
            gate,
            failed,
        }
    }

    /// The rest of C `dbDbPutValue` after its `dbPut`: the special's link
    /// writes and the rest of that `dbPut` (`LinkTail`), then the source's
    /// alarm inherited by the target (`dbDbLink.c:382-383`), then
    /// `processTarget` on a successful put (`:384-390`). Returns `true` when
    /// the put failed — C `dbPutLink` status `!= 0`.
    ///
    /// `source` is the writing record's frame (`None` only for a test's
    /// frameless write, which inherits nothing).
    fn finish_db_link_write(
        &self,
        write: DbLinkWrite,
        staged: &mut Option<LinkStage>,
        source: Option<InFrame<'_>>,
        src: OutLinkSrc<'_>,
        visited: &mut ProcStack,
    ) -> DbResult {
        let held = source.map(|source| source.set());
        let DbLinkWrite {
            target,
            switch,
            gate,
            mut failed,
        } = write;
        if staged.is_some()
            && let Some(rec) = &target
        {
            failed |= self.finish_link_stage(rec, staged, held, visited);
        }
        // C `dbDbPutValue` (`dbDbLink.c:382-383`) folds the source's PENDING
        // alarm into the destination after this put, whatever its status:
        // `psrce->nsta/nsev/namsg` read now, so an earlier link's
        // `setLinkAlarm` (`dbLink.c:445`) in the same cycle is part of
        // it. NMS (the common case) reads nothing.
        if switch != crate::server::record::MonitorSwitch::NoMaximize
            && let Some(rec) = &target
            && let Some(source) = source
        {
            let alarm = LinkAlarm::pending(&source.read().common);
            let mut tg = rec.write_under(held);
            inherit_sevr_msg(&mut tg.common, switch, &alarm);
        }
        // Only a successful write reaches target processing. An empty array
        // into a scalar field is not a failure: C `dbPut` raises LINK/INVALID
        // on the destination and returns 0 (`dbAccess.c:1370-1372`).
        // `dbPut`'s own failure codes are not kept; the put reports `-1`.
        if failed {
            return Err(DbStatus::FAILED);
        }
        // `dbDbPutValue` returns `processTarget`'s status (`dbDbLink.c:387-392`).
        if let (Some(gate), Some(rec)) = (gate, &target) {
            let src = TargetSrc {
                rec: source.map(|source| source.unframed()),
                putf: src.putf,
                notify: src.notify.wait_set(),
            };
            return self.process_target_at(src, rec, gate, held, visited);
        }
        Ok(())
    }

    /// C `dbPut`, or with `field_put` `dbPutField` (`dbAccess.c:1245-1290`),
    /// of `value` to `record.field`, made by device support from inside its
    /// own record's hold: the `dbPut` of a DB link's write
    /// ([`Self::write_local_target`], [`Self::finish_db_link_write`]), under
    /// `dbPutField`'s `dbScanLock(precord)`, which the caller's hold already
    /// is when the target shares its set.
    ///
    /// `dbPutField`'s process is its own, not `processTarget`'s: an active
    /// target takes RPRO with no `dbProcess` and so no LCNT count
    /// ([`super::field_io::db_process_claim`]). `dbPutField` hands a link
    /// field to `dbPutFieldLink` (`dbAccess.c:1261-1262`): from a dset
    /// call's hold that is [`Self::put_field_held`], settled under the
    /// caller's hold; an entry's hold hands it back instead
    /// ([`super::HeldInstance::db_put_field`]). `dbPut` refuses one.
    pub(crate) fn put_from_held(
        &self,
        record: &[u8],
        field: &str,
        value: EpicsValue,
        field_put: bool,
        visited: &mut ProcStack,
    ) -> DbResult {
        let Some((name, rec)) = self.lookup_record(record) else {
            return Err(DbStatus::FAILED);
        };
        let (field, at) =
            crate::server::record::record_instance::PutAddr::resolve_named(&rec, field)
                .map_err(|e| DbStatus::of_error(&e))?;
        if field_put && crate::types::dbf_link_class(rec.rdes().record_type, &field).is_some() {
            // Nested, as C's is under `dbProcess`'s `dbScanLock`:
            // `dbScanLockMany` refuses only its own locker locked twice
            // (`dbLock.c:392-393`).
            let (put, owed) = self.put_field_held(
                name.as_bytes(),
                &field,
                value,
                super::field_io::PutSplit::DbPutField,
            );
            owed.settle(self, name.as_bytes(), &field);
            return put.map_err(|e| DbStatus::of_error(&e));
        }
        let gate = super::FrameGate::of_instance(self, &rec);
        let frame = gate.frame(&rec);
        if field_put {
            super::field_io::check_put_disabled(&frame.read(), &field)
                .map_err(|e| DbStatus::of_error(&e))?;
        }
        let to = super::field_io::PutTarget {
            name: name.as_bytes(),
            rec: &rec,
            field: &field,
            at: &at,
        };
        let acts = TargetActs {
            gate: None,
            switch: crate::server::record::MonitorSwitch::NoMaximize,
        };
        let mut staged = StageSlot::default();
        let mut out = LinkWriteOut {
            staged: &mut staged.0,
            visited,
        };
        let write = self.write_local_target(
            Some(frame.set()),
            to,
            acts,
            super::field_io::PutSrc::Owned(value),
            &mut out,
        );
        // The target stands as its own source: with no MS and no gate, only
        // its set is read.
        let src = OutLinkSrc {
            putf: false,
            notify: OutNotify::None,
            field: "",
        };
        self.finish_db_link_write(write, out.staged, Some(frame), src, out.visited)?;
        if field_put
            && super::field_io::put_drives_processing_of(&rec, &frame.read(), &field)
            && super::field_io::db_process_claim(
                &rec,
                frame.set(),
                Some(super::field_io::PutSplit::DbPutField),
            )
        {
            return self
                .process_record_with_links_recursive_at(name.as_bytes(), &rec, visited)
                .map_err(|e| DbStatus::of_error(&e));
        }
        Ok(())
    }

    /// C `dbProcess(precord)` made by device support from inside its own
    /// record's hold, under the target's set as
    /// [`Self::put_from_held`] takes it.
    pub(crate) fn process_from_held(&self, record: &[u8], visited: &mut ProcStack) -> DbResult {
        let Some((name, rec)) = self.lookup_record(record) else {
            return Err(DbStatus::FAILED);
        };
        let _gate = super::FrameGate::of_instance(self, &rec);
        self.process_record_with_links_recursive_at(name.as_bytes(), &rec, visited)
            .map_err(|e| DbStatus::of_error(&e))
    }

    /// [`LinkStage`]'s owed work: the special's link writes, the rest of its
    /// `dbPut`, then the report. `true` when the put failed.
    #[cold]
    fn finish_link_stage(
        &self,
        rec: &Arc<crate::server::record::RecordCell>,
        staged: &mut Option<LinkStage>,
        held: Option<&super::SetGuard>,
        visited: &mut ProcStack,
    ) -> bool {
        let Some(LinkStage { stage, report }) = staged.take() else {
            return false;
        };
        let failed = self
            .finish_put_stage(rec.name(), rec, Some(stage), held, visited)
            .is_err();
        if let Some((tail, field)) = report {
            self.emit_cycle_report(rec.name(), &field, tail);
        }
        failed
    }

    /// Resolve an external OUT-link name to its put-queue target and ask C
    /// `dbCaPutLinkCallback`'s first gate about it — `if (!pca->isConnected ||
    /// !pca->hasWriteAccess) return -1;` (`db/dbCa.c:529-532`
    /// (`dbCaPutLinkCallback`); epics-base R7.0.10).
    ///
    /// The single owner of that gate. [`Self::write_external_pv`] runs it
    /// before it stages anything, and [`Self::external_put_admitted`] runs it
    /// alone for a caller that must know the status BEFORE the write is
    /// deferred — sseq's `WAITn`, whose C reads the return code of that very
    /// call to decide between `waiting = 1` and `abort = 1`
    /// (`sseqRecord.c:739-753`). One gate, so the answer the record acts on
    /// and the answer the write obeys cannot differ.
    fn external_put_gate<'a>(
        &self,
        link: crate::server::record::ExternalRef<'a>,
    ) -> Result<(&'static str, super::LinkRef<'a>), ExternalPutRefusal> {
        use super::link_put_queue::find_lset;
        use super::link_set::PutAdmission;
        let shown = String::from_utf8_lossy(link.name);

        let (scheme, lref) = Self::external_link_ref(link);
        if !self.link_addressable(scheme, lref.name) {
            return Err(ExternalPutRefusal::Unserviceable(format!(
                "no '{scheme}' link set registered for '{shown}'"
            )));
        }

        // A database built with no tokio runtime anywhere has no reactor for
        // the queue's network half, and there is no second executor that could
        // stand in (see `LinkPutQueue::network`). Refuse here, with nothing
        // staged, so the record takes its LINK alarm this cycle — C's shape
        // for "this put cannot be delivered", `dbCaPutLinkCallback` returning
        // `-1` before touching `workList` (`db/dbCa.c:529-532`
        // (`dbCaPutLinkCallback`); epics-base R7.0.10). C cannot reach the
        // no-owner state at all: `dbCaLinkInitImpl` creates the `dbCaLink`
        // worker unconditionally and blocks until it has started
        // (`db/dbCa.c:322` (`dbCaLinkInitImpl`), `:342`, `:344`; R7.0.10).
        if self.inner.link_puts.network().is_none() {
            return Err(ExternalPutRefusal::Unserviceable(format!(
                "external link '{shown}' refused the write: this database was \
                 built with no tokio runtime, so no link set can reach the \
                 network"
            )));
        }

        // A link that is known-down refuses the put and stages nothing, so the
        // record alarms in this cycle. `put_admission` is answered from cached
        // state — this is the only lset call left inside the record's advisory
        // write gate, and it does no I/O.
        let admission = find_lset(&self.inner, scheme, |lset| Some(lset.put_admission(lref)))
            .unwrap_or(PutAdmission::Refused);
        if admission == PutAdmission::Refused {
            return Err(ExternalPutRefusal::NotConnected);
        }
        Ok((scheme, lref))
    }

    /// Ask [`Self::external_put_gate`] on its own, staging nothing: `Ok` iff a
    /// put through `link` would be issued right now.
    pub(crate) fn external_put_admitted(
        &self,
        link: crate::server::record::ExternalRef<'_>,
    ) -> Result<(), ExternalPutRefusal> {
        self.external_put_gate(link).map(|_| ())
    }

    /// What an OUT put sends over an external link. A long string is C
    /// `dbPutLinkLS`'s (`dbLink.c:527-538`), whose `dbGetLinkDBFtype` is the
    /// channel's native type, or -1 while it is down (`pcaGetCheck`,
    /// `dbCa.c:621-631`): then nothing is put and the status is 0. A
    /// `DBF_CHAR`/`DBF_UCHAR` channel takes the characters; any other one
    /// `DBR_STRING`.
    fn external_put_value(
        &self,
        link: crate::server::record::ExternalRef<'_>,
        src: super::field_io::PutSrc<'_>,
    ) -> Option<EpicsValue> {
        let Some(chars) = src.long_string() else {
            return Some(src.into_value());
        };
        match self.external_link_metadata(link)?.dbf_type? {
            LinkDbfType::Char | LinkDbfType::UChar => Some(src.into_value()),
            _ => Some(EpicsValue::String(PvString::from(
                super::char_view::long_string_field_bytes(chars),
            ))),
        }
    }

    /// Write a value to an external (`ca://` / `pva://`) OUT link
    /// through the registered [`LinkSet`](super::LinkSet) — **by staging it on the
    /// database's link-put queue and returning**, exactly as C
    /// `dbCaPutLink` stages into `pca->pputNative` and returns
    /// (`dbCa.c:515-602`).
    ///
    /// This is the OUTPUT-side twin of [`Self::resolve_external_pv`]:
    /// the input side dispatches a `ParsedLink::Ca`/`Pva` read through
    /// `lset.get_value`, this dispatches a record's OUT-link write
    /// through `lset.put_value`. Mirrors C `dbLink.c::dbPutLink`
    /// (dbLink.c:434-448), which routes every link write — DB or CA —
    /// through `plink->lset->putValue` and raises a link alarm
    /// (`setLinkAlarm`) on failure.
    ///
    /// `name` may be a fully scheme-prefixed string (`pva://X`,
    /// `ca://X`) or the bare body (the form stored in
    /// `ParsedLink::Ca`/`Pva` after `record/link.rs` strips the
    /// scheme), which is a CA link.
    ///
    /// # What the returned status means
    ///
    /// The **staging** status, which is what C returns: `dbCaPutLink`'s
    /// value is the conversion status or the `-1` refusal of
    /// `dbCa.c:529-532`, never the outcome of the wire write (that reaches
    /// the operator through `errlogPrintf` on the `dbCaTask`,
    /// `dbCa.c:1175-1179`). So:
    ///
    /// * `Err` — no lset is registered for the scheme, or the lset reports
    ///   the link as [`PutAdmission::Refused`](super::PutAdmission::Refused). The caller folds this
    ///   into the owning record's LINK/INVALID (`setLinkAlarm`), same cycle,
    ///   same as before this change.
    /// * `Ok(())` for [`LinkPutOp::Plain`](super::LinkPutOp::Plain) — the write is staged. It is
    ///   **not** yet on the wire. This is the fire-and-forget flavour, C's
    ///   `CA_PUT` / `ca_array_put` (`dbCa.c:1163-1166`), and it is the whole
    ///   point of the queue: record processing no longer suspends on a
    ///   `ca://`/`pva://` round trip inside the record's advisory write gate.
    /// * `Ok(())` for [`LinkPutOp::Async`](super::LinkPutOp::Async) too — the completion flavour, C's
    ///   `CA_PUT_CALLBACK` / `ca_array_put_callback` whose `putComplete`
    ///   drives the originating record's completion (`dbCa.c:1167-1171`,
    ///   `:994-1012`). `dbCaPutLinkCallback` stores the callback in
    ///   `pca->putCallback`, calls `addAction` and **returns**
    ///   (`dbCa.c:585-595`); what keeps the put-notify chain outstanding is
    ///   the RECORD staying active, not a held lock. So this call joins the
    ///   source record's [`NotifyWaitSet`] (C `dbNotifyAdd`) before staging
    ///   and hands the completion receiver to a spawned waiter that `leave`s
    ///   the set when the queue owner resolves it (C `dbNotifyCompletion`).
    ///   Nothing is awaited here — the record's advisory write gate is
    ///   released at the end of this cycle exactly as it is for a plain put.
    ///
    /// `done` names who the completion is reported to, and so selects the
    /// flavour: [`ExternalDone::None`] is the plain put, a put-notify chain's
    /// included; an explicit `dbCaPutLinkCallback` and a
    /// `devXxxSoftCallback` record ([`ExternalDone::Record`], whose
    /// completing pass `dbCaCallbackProcess` runs) take the completion one.
    pub(crate) fn write_external_pv(
        &self,
        link: super::LinkId,
        external: crate::server::record::ExternalRef<'_>,
        value: EpicsValue,
        done: ExternalDone<'_>,
    ) -> Result<(), ExternalPutRefusal> {
        use super::link_put_queue::{Channel, LinkKey, PutWaiter};

        let (scheme, lref) = self.external_put_gate(external)?;
        // `dbCaPutLinkCallback` converts into `pputNative` on the record
        // thread and returns that status (`dbCa.c:572-590`). Only a string
        // can fail to convert, so only a string is converted here; the lset
        // converts any other value when it sends it.
        let value = if matches!(value, EpicsValue::String(_) | EpicsValue::StringArray(_)) {
            match super::link_put_queue::find_lset(&self.inner, scheme, |lset| {
                lset.put_native(lref)
            }) {
                Some((native, nelements)) => {
                    super::link_put_convert(lref.name, value, native, nelements)
                        .map_err(ExternalPutRefusal::Unconvertible)?
                }
                None => value,
            }
        } else {
            value
        };

        let key = LinkKey {
            link,
            channel: Channel::new(scheme, lref.name, external.open),
        };
        let waiter = match done {
            ExternalDone::None => PutWaiter::None,
            ExternalDone::Notify(_) => PutWaiter::Notify,
            // A local target's completion reaches `dbCaCallbackProcess`
            // through dbNotify's callDone, pinned to priorityLow
            // (`dbNotify.c:129-131`, `:466`, `:182`); a remote one through
            // libca's callback thread.
            ExternalDone::Record(done) => {
                let band = if scheme == "ca" && self.inner.ca_links.is_local(lref) {
                    crate::runtime::task::CallbackPriority::Low
                } else {
                    done.priority()
                };
                PutWaiter::Record(done, band)
            }
        };
        match self.inner.link_puts.stage_put(key, value, waiter) {
            // `dbCaPutLink`: staged, signalled, returned (`dbCa.c:593-595`);
            // `dbCaPutAsync` the same, with the record's callback stored.
            None => Ok(()),
            // `dbCaPutLinkCallback`: the completion route. C stores the
            // callback and returns (`dbCa.c:585-595`); `putComplete`
            // (`dbCa.c:994-1012`) fires it later from the CA task. The
            // record-side counterpart of that callback is the put-notify
            // wait-set: join it now (C `dbNotifyAdd`) and let a spawned
            // waiter leave it when the owner resolves the completion (C
            // `dbNotifyCompletion`). The source record's own count is still
            // held by the cycle that issued this write, so the set cannot
            // drain between staging and joining.
            //
            // The returned status stays the STAGING status, never the
            // network status — `dbCaPutLinkCallback` returns the conversion
            // status and reports wire failures only through the task
            // (`dbCa.c:1175-1179`), and `dbPutLink`'s `setLinkAlarm` gate
            // therefore alarms on refusal to stage (`dbLink.c:434-448`).
            Some(rx) => {
                if let ExternalDone::Notify(waitset) = done {
                    let waitset = waitset.clone();
                    waitset.enter();
                    // C pins every put-notify callback to the low band —
                    // `callbackSetPriority(priorityLow, &pnotifyPvt->callback)`
                    // (`dbNotify.c:131`) — regardless of the record's PRIO.
                    crate::runtime::task::spawn_background(
                        crate::runtime::task::CallbackPriority::Low,
                        async move {
                            let _ = rx.await;
                            waitset.leave();
                        },
                    );
                }
                Ok(())
            }
        }
    }

    /// Drain the external-link put queue — C `dbCaSync`
    /// (`dbCa.c:1126-1129`, the `CA_SYNC` action the `dbCaTask` answers only
    /// once everything queued ahead of it has been serviced).
    ///
    /// Returns when every staged write has been handed to its lset and that
    /// lset's `put_value` has returned. The barrier a caller needs when it
    /// must observe the effect of a [`LinkPutOp::Plain`](super::LinkPutOp::Plain) write, which by
    /// construction is no longer complete when `write_external_pv` returns.
    pub async fn sync_external_link_puts(&self) {
        self.inner.link_puts.sync().await;
    }

    /// Number of staged external-link writes a later write on the same link
    /// overwrote — C's `pca->nNoWrite` (`dbCa.c:582-583`).
    pub fn external_link_puts_coalesced(&self) -> u64 {
        self.inner.link_puts.coalesced_count()
    }

    /// Number of external-link writes the queue owner has completed.
    pub fn external_link_puts_completed(&self) -> u64 {
        self.inner.link_puts.completed_count()
    }

    /// Fire a forward link whose target is an external PV, C `dbScanFwdLink`
    /// → `plink->lset->scanForward` (`dbLink.c:475-480`). Returns the lset's
    /// `Err` unchanged so the caller can raise LINK/INVALID on the owning
    /// record (pvxs `recGblSetSevrMsg(LINK_ALARM, INVALID_ALARM)`).
    ///
    /// A CA link is dbCa's `scanForward` (`dbCa.c:643-648`): a plain
    /// `dbCaPutLink` of 1 when `pvlOptFWD` is set, nothing otherwise, and its
    /// status is dropped, so it never alarms. A `pva` link runs the handle
    /// its lset opened it with ([`Self::bind_external_forward`]).
    pub(crate) fn scan_forward_external_pv(
        &self,
        link: super::LinkId,
        target: crate::server::record::ExternalForward<'_>,
    ) -> Result<(), super::LinkForwardRefused> {
        use crate::server::record::ExternalForward;
        match target {
            ExternalForward::Ca {
                link: external,
                fwd,
            } => {
                if fwd {
                    let _ = self.write_external_pv(
                        link,
                        external,
                        EpicsValue::Short(1),
                        ExternalDone::None,
                    );
                }
                Ok(())
            }
            ExternalForward::Pva(slot) => slot.scan_forward(),
        }
    }

    /// Resolve an OUT link's TARGET metadata — the DBF field type, the
    /// element capacity, and whether C would classify the link as a
    /// `CA_LINK`. Single owner of the target-metadata lookup every soft
    /// device support's write-buffer switch reads
    /// ([`Record::multi_output_buffer`](crate::server::record::Record::multi_output_buffer)).
    ///
    /// C's resolution, mirrored branch for branch:
    /// - local DB target — `dbNameToAddr` (`devsCalcoutSoft.c:127-131`):
    ///   `field_type` = the target field's DBF type, `no_elements` = its
    ///   capacity. An unresolvable name leaves C's initializers
    ///   (`field_type = 0`, `n_elements = 1`) — the port reports
    ///   [`OutTarget::UNRESOLVED`], whose `None` type routes to the device
    ///   support's `default:` arm.
    /// - external target (explicit `ca://`/`pva://` OR a DB-parsed name not
    ///   in this IOC, which C classifies as a CA link) —
    ///   `dbCaGetLinkDBFtype` / `dbCaGetNelements` (`dbCa.c:633-675`,
    ///   both `-1` while disconnected): the lset's cached
    ///   [`LinkMetadata`](super::LinkMetadata), `UNRESOLVED` when the link is down.
    ///
    /// `no_elements` is a field CAPACITY, which the port does not carry per
    /// field: an array field reports its record's `NELM` when it has one and
    /// its current length otherwise; a scalar field reports 1.
    pub(crate) fn resolve_out_target(&self, link: &crate::server::record::ParsedLink) -> OutTarget {
        let external = |link: crate::server::record::ExternalRef<'_>| {
            let put_admitted = self.external_put_admitted(link).is_ok();
            match self.external_link_metadata(link) {
                // A remote channel reports the DBR type its DBF class is
                // served as (`dbCaGetLinkDBFtype` → the channel's type): a
                // remote `DBF_MENU` or link field arrives as `DBF_ENUM` or
                // `DBF_STRING`.
                Some(m) => OutTarget {
                    field_type: m.dbf_type.map(|t| link_dbf_to_field_type(t).dbf_code()),
                    dbr_field_type: m.dbf_type.map(link_dbf_to_field_type),
                    element_count: m.element_count.unwrap_or(1).max(1),
                    is_ca_link: true,
                    constant_type: false,
                    put_admitted,
                },
                None => OutTarget {
                    is_ca_link: true,
                    constant_type: false,
                    put_admitted,
                    ..OutTarget::UNRESOLVED
                },
            }
        };
        match link {
            crate::server::record::ParsedLink::Db(db) => {
                // A DB link is never `CONSTANT`, addressable or not.
                const UNADDRESSED: OutTarget = OutTarget {
                    constant_type: false,
                    ..OutTarget::UNRESOLVED
                };
                let addressed = db.target();
                let target = match self.db_link_target(db) {
                    LinkTarget::Local(rec) => rec,
                    LinkTarget::LocalNotRecord => return UNADDRESSED,
                    // Non-local ⇒ CA link in C (`dbInitLink` locality).
                    LinkTarget::External => return external(db.external()),
                };
                let guard = target.read();
                // `dbNameToAddr` of a `$` address: a string's `DBF_CHAR` view,
                // or a link's own type, read as `DBR_CHAR`, and its
                // `no_elements`.
                if addressed.string_view {
                    return match super::char_view::char_view(&guard, &addressed.field) {
                        Some(view) => OutTarget {
                            field_type: Some(view.field_type),
                            dbr_field_type: Some(DbFieldType::Char),
                            element_count: (view.no_elements as i64).max(1),
                            is_ca_link: false,
                            constant_type: false,
                            put_admitted: true,
                        },
                        None => UNADDRESSED,
                    };
                }
                let field_type = self.addressed_field_type(
                    &target,
                    &addressed.field,
                    Some(guard.record.as_ref()),
                );
                let element_count = match guard.record.get_field(&addressed.field) {
                    Some(v) if v.is_array() => guard
                        .record
                        .get_field("NELM")
                        .and_then(|n| n.as_int_i64())
                        .filter(|n| *n > 0)
                        .unwrap_or(v.count() as i64),
                    _ => 1,
                };
                OutTarget {
                    field_type,
                    dbr_field_type: field_type.and_then(crate::types::DbfCode::served_type),
                    element_count: element_count.max(1),
                    is_ca_link: false,
                    constant_type: false,
                    put_admitted: true,
                }
            }
            crate::server::record::ParsedLink::Ca(_)
            | crate::server::record::ParsedLink::Pva(_)
            | crate::server::record::ParsedLink::PvaJson(_) => {
                external(link.external().expect("Ca/Pva/PvaJson link is external"))
            }
            // C `lnkState_getDBFtype`/`lnkState_getElements`
            // (`lnkState.c:126-135`): one `DBF_SHORT` element, and never a
            // string.
            crate::server::record::ParsedLink::State(_) => OutTarget {
                field_type: Some(crate::types::DbfCode::Short),
                dbr_field_type: Some(DbFieldType::Short),
                element_count: 1,
                is_ca_link: false,
                constant_type: false,
                put_admitted: true,
            },
            // Constant / Hw / Calc / None / Removed: no target. The type is
            // the one the link holds — a removed DB link's is `PV_LINK`.
            _ => OutTarget {
                constant_type: link.db_link_type() == crate::server::record::DbLinkType::Constant,
                ..OutTarget::UNRESOLVED
            },
        }
    }

    /// Apply the writing record's device-support write-buffer switch to a
    /// multi-output pair: resolve the TARGET ([`Self::resolve_out_target`]),
    /// then let the record pick the buffer C's `write_*` would put
    /// ([`Record::multi_output_buffer`](crate::server::record::Record::multi_output_buffer)).
    ///
    /// The record lock is NOT held across the target resolution — a
    /// self-referencing OUT link would re-enter the source record's own
    /// gate — so the target is resolved first and the record re-read to
    /// make the pick, exactly as C's device support reads its own fields
    /// after `dbNameToAddr` / `dbCaGet*`.
    pub(crate) fn multi_out_buffer_choice(
        &self,
        rec: InFrame<'_>,
        link_field: &str,
        link: &crate::server::record::ParsedLink,
        staged: EpicsValue,
    ) -> EpicsValue {
        let target = self.resolve_out_target(link);
        let guard = rec.read();
        guard
            .record
            .multi_output_buffer(link_field, staged, &target)
    }

    /// The record's generic multi-output OUT writes — scalcout / acalcout
    /// `OUT`->`OVAL`, the pairs a record declares through
    /// [`Record::multi_output_links`](crate::server::record::Record::multi_output_links). C's soft device support performs these
    /// from `writeValue`, i.e. inside `process()` and BEFORE `monitor()`
    /// commits the cycle's alarm, so this runs pre-commit: a failed put's
    /// `LINK_ALARM`/`INVALID` (raised inside [`Self::write_out_link_value`])
    /// lands in the SAME cycle's committed SEVR and monitor posts.
    ///
    /// SINGLE-OWNER INVARIANT: a record type whose link groups are dispatched
    /// by [`Self::dispatch_multi_output`] (fanout/dfanout/seq) MUST be skipped
    /// here — otherwise its `LNKn`/`OUTn` would be written twice per cycle.
    /// `sseq` once implemented `Record::multi_output_links` as well and was
    /// double-dispatched; the `multi_output_dispatch_owned` gate makes that
    /// structurally impossible rather than fixed at the record.
    ///
    /// Inline so the no-op answer of every type without pairs costs no call.
    #[inline(always)]
    pub(crate) fn dispatch_multi_output_values(
        &self,
        rec: InFrame<'_>,
        src: OutLinkSrc<'_>,
        skip_out: bool,
        plan: &crate::server::record::record_instance::ProcessPlan,
        visited: &mut ProcStack,
    ) {
        // IVOA=Don't_drive veto (execOutput `nsev >= INVALID` → Don't_drive
        // `break`, sCalcoutRecord.c:794). `skip_out` is the decision the
        // cycle's single IVOA owner already made, on the severity
        // `checkAlarms` produced — re-deriving it here from `nsev` would read
        // an alarm the OUT write above may have raised itself (a failed put's
        // LINK_ALARM/INVALID), acting on a veto C never applied.
        //
        // Both tests precede the record lock. Whether this type has generic
        // multi-output pairs at all is settled once in `ProcessPlan`; asking
        // it here meant every record in the database — a `calc` with no OUT
        // included — took the lock once a cycle to be told its type name is
        // not one of four.
        if skip_out || !plan.dispatches_generic_multi_output {
            debug_assert!(
                skip_out || rec.read().record.multi_output_links().is_empty(),
                "a type whose multi_output_links can be non-empty must answer \
                 declares_multi_output_links(); otherwise its outputs vanish in release"
            );
            return;
        }
        self.write_multi_output_pairs(rec, src, visited);
    }

    /// [`Self::dispatch_multi_output_values`] past its gate, out of line so a
    /// type the gate turns away pays a branch and not this frame's prologue.
    fn write_multi_output_pairs(
        &self,
        rec: InFrame<'_>,
        src: OutLinkSrc<'_>,
        visited: &mut ProcStack,
    ) {
        let pairs = {
            let instance = rec.read();
            debug_assert!(
                !multi_output_dispatch_owned(instance.record.record_type()),
                "a fanout/dfanout/seq array is dispatch_multi_output's, not the generic block's"
            );
            let links = instance.record.multi_output_links();
            let mut pairs = Vec::new();
            for &(link_field, val_field) in links {
                let link = match instance.stored_link_shared(link_field) {
                    None => continue,
                    Some(link) if matches!(**link, crate::server::record::ParsedLink::None) => {
                        continue;
                    }
                    Some(link) => link,
                };
                if let Some(val) = instance.record.get_field(val_field) {
                    pairs.push((link_field, Arc::clone(link), val));
                }
            }
            pairs
        };
        for (link_field, parsed, val) in pairs {
            // `multi_output_links` carries record OUT links (scalcout /
            // acalcout `OUT` — `DBF_OUTLINK`) driven via `dbPutLink` →
            // `dbDbPutValue` (`dbDbLink.c:388`): a bare DB link is NPP, the
            // value is written but the target is NOT processed: the slot
            // parsed the field under the OUT-link mask, whose NPP default
            // that is. An external `ca://`/`pva://` OUT link routes through
            // the link set's `putValue` (C `dbLink.c::dbPutLink`,
            // dbLink.c:434-448).
            // Device-support write-buffer switch: the resolved target's DBF
            // type / element count decides which of the record's buffers C
            // would actually put (`devsCalcoutSoft.c:66-144`,
            // `devaCalcoutSoft.c:75-87`).
            let val = self.multi_out_buffer_choice(rec, link_field, &parsed, val);
            // scalcout, acalcout, aSub and epid return 0 past these puts.
            let _ = self.write_out_link_value(
                rec,
                parsed,
                super::PutSrc::Owned(val),
                OutLinkSrc {
                    field: link_field,
                    ..src
                },
                visited,
            );
        }
    }

    /// **The put owner** — C `dbLink.c::dbPutLink` (434-448). Writes a value
    /// through a parsed OUT link, dispatching DB links to
    /// [`Self::write_db_link_value_in`] and external (`ca://`/`pva://`) links to
    /// [`Self::write_external_pv`], and — this is the part no caller may skip
    /// — raising the source record's `LINK_ALARM`/`INVALID` when the put
    /// fails:
    ///
    /// ```c
    /// status = plset->putValue(plink, dbrType, pbuffer, nRequest);
    /// if (status) {
    ///     setLinkAlarm(plink);        /* LINK_ALARM / INVALID_ALARM */
    /// }
    /// ```
    ///
    /// INVARIANT: *every* failing OUT-link put alarms the writing record, and
    /// the alarm is raised HERE, inside the put — not by each caller. C puts
    /// it inside `dbPutLink` for exactly that reason (the async twin
    /// `dbPutLinkAsync`, `:469-471`, repeats it), so a record whose OUT/`OUTn`
    /// target is down goes INVALID no matter which code path issued the write.
    /// The alarm lands in the record's PENDING alarm, so the cycle's
    /// `rec_gbl_reset_alarms` — which C runs from `monitor()`, AFTER the
    /// record's output writes — commits it in the SAME cycle.
    ///
    /// `src.field` names the link field for the alarm message, as C's
    /// `setLinkAlarm` does (`"field %s"`, `dbLinkFieldName(plink)`).
    ///
    /// This is also the OUTPUT-side counterpart of [`Self::read_link_value_and_alarm_at_source`]'s
    /// scheme dispatch: the OUT-link write stage in `processing.rs`
    /// must route a `ParsedLink::Ca`/`Pva` through the link set, not
    /// only handle `ParsedLink::Db`.
    ///
    /// A `Constant`/`None` OUT link takes the put as a successful no-op
    /// (`dbConstPutValue`). A `Hw`/`Removed` one has no lset, so the put
    /// fails `S_db_noLSET` without a `setLinkAlarm` (`dbLink.c:440-441`).
    ///
    /// Returns C's `dbPutLink` status: a soft dset returns it from its
    /// `write_xxx`, and dfanout raises its own `LINK_ALARM`/`MAJOR` on top
    /// of it. The INVALID that every failing put owes the record is already
    /// raised here.
    #[inline]
    pub(crate) fn write_out_link_value(
        &self,
        src_rec: InFrame<'_>,
        link: Arc<crate::server::record::ParsedLink>,
        value: super::field_io::PutSrc<'_>,
        src: OutLinkSrc<'_>,
        visited: &mut ProcStack,
    ) -> DbResult {
        let mut alarms = true;
        let status = match &*link {
            crate::server::record::ParsedLink::Db(_) => {
                let mut staged = StageSlot::default();
                let mut out = LinkWriteOut {
                    staged: &mut staged.0,
                    visited,
                };
                let write = self.write_db_link_value_in(Some(src_rec), link, value, &src, &mut out);
                self.finish_db_link_write(write, out.staged, Some(src_rec), src, out.visited)
            }
            crate::server::record::ParsedLink::Ca(_)
            | crate::server::record::ParsedLink::Pva(_)
            | crate::server::record::ParsedLink::PvaJson(_) => {
                let external = link
                    .external()
                    .expect("Ca/Pva/PvaJson link carries a PV name");
                match self.external_put_value(external, value) {
                    None => Ok(()),
                    Some(value) => {
                        match self.write_external_pv(
                            src.link_id(Some(src_rec)),
                            external,
                            value,
                            src.external_done(),
                        ) {
                            Ok(()) => Ok(()),
                            // `dbCaPutLinkCallback`'s and pvalink's `-1`.
                            Err(refusal) => {
                                refusal.report(external.name);
                                Err(DbStatus::FAILED)
                            }
                        }
                    }
                }
            }
            // C `lnkState_putValue` (`lnkState.c:153-203`) drives the named
            // bit; a DBR class it has no arm for is `S_db_badDbrtype`, which
            // `dbPutLink` turns into the link alarm and nothing printed
            // (`dbLink.c:443-446`).
            crate::server::record::ParsedLink::State(st) => {
                use crate::server::record::{StateLink, StatePut};
                let owned;
                let value = match value.value() {
                    Some(v) => v,
                    None => {
                        owned = value.into_value();
                        &owned
                    }
                };
                match StateLink::put_of(value) {
                    StatePut::Truth(truth) => {
                        self.db_state(&st.name).set(st.write(truth));
                        Ok(())
                    }
                    StatePut::Nothing => Ok(()),
                    StatePut::BadDbrType => Err(DbStatus::BAD_DBRTYPE),
                }
            }
            crate::server::record::ParsedLink::Calc(calc) => {
                match self.put_calc_link(src_rec, calc, value, src, visited) {
                    true => Err(DbStatus::FAILED),
                    false => Ok(()),
                }
            }
            // A constant link's `dbConstPutValue` answers 0
            // (`dbConstLink.c:228-232`). A hardware or removed link has no
            // lset: `S_db_noLSET`, and no `setLinkAlarm` (`dbLink.c:440-441`).
            other => {
                alarms = false;
                match crate::server::recgbl::simm::has_no_lset(other) {
                    true => Err(DbStatus::NO_LSET),
                    false => Ok(()),
                }
            }
        };
        if alarms && status.is_err() {
            set_out_link_alarm(src_rec, src.field);
        }
        status
    }

    /// C `dbPutLinkAsync` (`dbLink.c:462-474`) of a `devXxxSoftCallback`
    /// write, under the record's guard. Only a CA link has a `putAsync` — a
    /// `ca://`/`pva://` link, or a DB link whose target is not here — and it
    /// only stages the put, so no other record is locked. Any other link
    /// answers `S_db_noLSET` for the caller's `dbPutLink`. A refused
    /// `putAsync` alarms as `dbPutLinkAsync` does (`:469-471`).
    #[cold]
    #[inline(never)]
    pub(crate) fn put_out_async(
        &self,
        rec: InFrame<'_>,
        instance: &mut crate::server::record::RecordInstance,
    ) -> AsyncPutStatus {
        use crate::server::record::{DbLinkAnswer, ParsedLink};
        let link = Arc::clone(instance.parsed_out.shared());
        let external = match &*link {
            ParsedLink::Ca(_) | ParsedLink::Pva(_) | ParsedLink::PvaJson(_) => link.external(),
            ParsedLink::Db(db) if matches!(self.db_link_answer(db), DbLinkAnswer::External) => {
                Some(db.external())
            }
            _ => None,
        };
        let Some(external) = external else {
            return AsyncPutStatus::NoLset;
        };
        let value = super::field_io::PutSrc::of_output_held(instance, true)
            .and_then(|src| self.external_put_value(external, src));
        let Some(value) = value else {
            return AsyncPutStatus::Done;
        };
        let done = instance.device_done.clone();
        let link_id = super::LinkId {
            rec: rec.unframed().index(),
            field: "OUT",
            install: super::LinkInstall::of_field(instance, "OUT"),
        };
        match self.write_external_pv(link_id, external, value, ExternalDone::Record(&done)) {
            Ok(()) => AsyncPutStatus::Taken,
            Err(refusal) => {
                refusal.report(external.name);
                crate::server::recgbl::rec_gbl_set_link_alarm(&mut instance.common, "OUT");
                AsyncPutStatus::Refused
            }
        }
    }

    /// [`Self::write_out_link_value`] for the write a frame captured.
    #[inline(always)]
    pub(crate) fn write_out(
        &self,
        src_rec: InFrame<'_>,
        out: OutLinkWrite,
        value: super::field_io::PutSrc<'_>,
        src: OutLinkSrc<'_>,
        visited: &mut ProcStack,
    ) -> DbResult {
        match out.0 {
            OutWrite::Lent(ref lent) => {
                self.write_lent_out(src_rec, lent, value, src, visited)?;
                match lent.acts.gate {
                    None => Ok(()),
                    Some(gate) => self.process_lent_target(src_rec, lent, gate, src, visited),
                }
            }
            OutWrite::Link(link) => self.write_out_link_value(src_rec, link, value, src, visited),
        }
    }

    /// [`Self::write_out`] for a hop's frame: a lent target's
    /// `processTarget` is handed back, for the frame to take once it has
    /// parked ([`PvDatabase::drive_hops`]).
    #[inline(always)]
    pub(crate) fn write_out_hop(
        &self,
        src_rec: InFrame<'_>,
        out: OutLinkWrite,
        value: super::field_io::PutSrc<'_>,
        src: OutLinkSrc<'_>,
        visited: &mut ProcStack,
    ) -> Result<Option<OutHop>, DbStatus> {
        match out.0 {
            OutWrite::Lent(ref lent) => {
                self.write_lent_out(src_rec, lent, value, src, visited)?;
                Ok(lent.acts.gate.map(|gate| OutHop {
                    target: lent.target,
                    gate,
                }))
            }
            OutWrite::Link(link) => self
                .write_out_link_value(src_rec, link, value, src, visited)
                .map(|()| None),
        }
    }

    /// C `dbDbPutValue` at the lent target, which needs no link, up to its
    /// `processTarget`: [`Self::process_lent_target`] takes that once this
    /// frame, about 2 KB, is popped, or an OUT PP chain holds it per hop.
    fn write_lent_out(
        &self,
        src_rec: InFrame<'_>,
        lent: &LentOut,
        value: super::field_io::PutSrc<'_>,
        src: OutLinkSrc<'_>,
        visited: &mut ProcStack,
    ) -> DbResult {
        let held = src_rec.set();
        // SAFETY: `OutLinkWrite::capture` read the target from this frame's
        // record's OUT cache at the revision it checked.
        let rec = unsafe { lent.target.lend(held, src_rec.unframed()) };
        let put = lent.target.put();
        let to = super::field_io::PutTarget {
            name: rec.name(),
            rec: &rec,
            field: lent.field.name,
            at: &put,
        };
        let mut staged = StageSlot::default();
        let mut out = LinkWriteOut {
            staged: &mut staged.0,
            visited,
        };
        let acts = TargetActs {
            gate: None,
            ..lent.acts
        };
        let write = self.write_local_target(Some(held), to, acts, value, &mut out);
        let status = self.finish_db_link_write(write, out.staged, Some(src_rec), src, out.visited);
        if status.is_err() {
            set_out_link_alarm(src_rec, src.field);
        }
        status
    }

    /// C `dbDbPutValue`'s `processTarget` (`dbDbLink.c:384-390`) after a
    /// successful [`Self::write_lent_out`], and `dbPutLink`'s alarm when it
    /// fails.
    #[inline(never)]
    fn process_lent_target(
        &self,
        src_rec: InFrame<'_>,
        lent: &LentOut,
        gate: ProcessTargetGate,
        src: OutLinkSrc<'_>,
        visited: &mut ProcStack,
    ) -> DbResult {
        let held = src_rec.set();
        // SAFETY: as in `write_lent_out`; the put since was a `dbPut`, which
        // refuses a link field, so it left the OUT cache as the frame read it.
        let rec = unsafe { lent.target.lend(held, src_rec.unframed()) };
        let target_src = TargetSrc {
            rec: Some(src_rec.unframed()),
            putf: src.putf,
            notify: src.notify.wait_set(),
        };
        let status = self.process_target_at(target_src, &rec, gate, Some(held), visited);
        if status.is_err() {
            set_out_link_alarm(src_rec, src.field);
        }
        status
    }

    /// C `lnkCalc_putValue` (`lnkCalc.c:631-703`): the value, or `expr`
    /// over the args when there is one, is checked by `major` then `minor`
    /// and put through `out`. What the input reads raise lands on the record
    /// first, whatever comes after. `true` is a non-zero status.
    #[cold]
    #[inline(never)]
    fn put_calc_link(
        &self,
        src_rec: InFrame<'_>,
        calc: &crate::server::record::CalcLink,
        value: super::field_io::PutSrc<'_>,
        src: OutLinkSrc<'_>,
        visited: &mut ProcStack,
    ) -> bool {
        use crate::server::record::AlarmSeverity;
        let Some((vars, mut raised)) = self.calc_link_args(calc, Some(src_rec.unframed())) else {
            return true;
        };
        calc.val.set_post_sevr(AlarmSeverity::NoAlarm);
        // C `calcPerform(clink->arg, &val, expr)`: `VAL` reads the value
        // the expression starts from.
        let perform = |expr: &str, val: f64| {
            let compiled = crate::calc::compile(expr).ok()?;
            let mut inputs = crate::calc::engine::NumericInputs::with_vars(vars);
            inputs.prev_val = val;
            crate::calc::eval(&compiled, &mut inputs).ok()
        };
        // C converts `pbuffer[0]` (`lnkCalc.c:672`).
        let val = link_value_as_f64(value.into_value())
            .and_then(|val| match &calc.expr {
                Some(expr) => perform(expr, val),
                None => Some(val),
            })
            .and_then(|val| {
                calc.val.set(val);
                // C copies "post_major error" into the link's own message
                // for `minor` too (`lnkCalc.c:696`); the record's message is
                // right.
                for (expr, sevr, msg) in [
                    (&calc.major, AlarmSeverity::Major, "post_major error"),
                    (&calc.minor, AlarmSeverity::Minor, "post_minor error"),
                ] {
                    let Some(expr) = expr else { continue };
                    if perform(expr, val)? != 0.0 {
                        calc.val.set_post_sevr(sevr);
                        let alarm = LinkAlarm {
                            stat: crate::server::recgbl::alarm_status::LINK_ALARM,
                            sevr,
                            amsg: msg.into(),
                        };
                        raise(&mut raised, alarm);
                        break;
                    }
                }
                Some(val)
            });
        if let Some(alarm) = raised {
            let mut inst = src_rec.write();
            crate::server::recgbl::rec_gbl_set_sevr_msg(
                &mut inst.common,
                alarm.stat,
                alarm.sevr,
                alarm.amsg,
            );
        }
        let Some(val) = val else {
            return true;
        };
        let Some(out) = &calc.out else {
            // A zeroed `clink->out` has no lset: `S_db_noLSET`.
            return true;
        };
        // The child is no field of the record, so the alarm its failed put
        // raises names `"????"` (`dbLinkFieldName`, `dbLink.c:52-66`); the
        // parent's `setLinkAlarm` after it is a no-op, the record being
        // INVALID already.
        let unalarmed = src_rec.read().common.nsev < AlarmSeverity::Invalid;
        let failed = self
            .write_out_link_value(
                src_rec,
                Arc::clone(out),
                super::field_io::PutSrc::Owned(EpicsValue::Double(val)),
                src,
                visited,
            )
            .is_err();
        if failed && unalarmed {
            let mut inst = src_rec.write();
            if inst.common.nsev == AlarmSeverity::Invalid {
                inst.common.namsg = crate::server::record::SparseText::link_field(CHILD_LINK_FIELD);
            }
        }
        failed
    }

    /// What a `{state:…}` link reads.
    ///
    /// C `lnkState_getValue` (`lnkState.c:139-151`) hands over
    /// `slink->invert ^ dbStateGet(slink->state)` through the `DBR_SHORT`
    /// fast-convert row, and `lnkState_getDBFtype` (`:126-129`) says
    /// `DBF_SHORT` with one element — so the link reads as a one-element
    /// short, never a bool and never a string.
    fn read_state_link(&self, st: &crate::server::record::StateLink) -> EpicsValue {
        EpicsValue::Short(i16::from(st.read(self.db_state(&st.name).get())))
    }

    /// The named `dbState` a `{state:…}` link addresses.
    ///
    /// The link's open ([`Self::open_json_link`]) creates it; this finds it
    /// as `dbStateCreate` does (`dbState.c:50-66`). The registry is the
    /// process-wide one the `sync` channel filter and the `Db State` device
    /// support already share, so all three see one bit per name.
    fn db_state(&self, name: &str) -> Arc<crate::server::database::filters::sync::DbState> {
        crate::server::database::filters::sync::db_state_registry().get_or_create(name)
    }

    /// Read a record numeric field as `i16`, defaulting to 0.
    fn field_i16(instance: &RecordInstance, field: &str) -> i16 {
        instance
            .record
            .get_field(field)
            .and_then(|v| v.to_f64())
            .unwrap_or(0.0) as i16
    }

    /// Read a record numeric field as `u16`, defaulting to 0.
    ///
    /// Used for `DBF_USHORT` fields such as `SELN`: the native unsigned
    /// carrier returns its value directly, while any other DBR type is
    /// truncated through the `i64` integer view (C `dbPut` cast), so a
    /// value ≥ 32768 round-trips instead of saturating at `i16::MAX`.
    fn field_u16(instance: &RecordInstance, field: &str) -> u16 {
        match instance.record.get_field(field) {
            Some(EpicsValue::UShort(v)) => v,
            Some(other) => other.as_int_i64().unwrap_or(0) as u16,
            None => 0,
        }
    }

    /// **The single owner of the SELL→SELN refresh** — C's
    /// `dbGetLink(&prec->sell, DBR_USHORT, &prec->seln, 0, 0)`.
    ///
    /// The three records that own a SELL read it at three different points,
    /// and the table below is the whole rule; `phase` is what makes "exactly
    /// once per cycle" readable in one place instead of split across two
    /// call-site gates:
    ///
    /// * **dfanout** — `dfanoutRecord.c:126`, between `recGblGetTimeStamp` and
    ///   `checkAlarms` (`:127`). The position MATTERS: a failed read is a
    ///   `setLinkAlarm`, and `:128` `if (prec->nsev < INVALID_ALARM)` is the
    ///   very next line, so a dead SELL sends the cycle down the IVOA branch.
    ///   Reading it in the output dispatch — after the alarms and after the
    ///   IVOA decision — drove the outputs of a record C vetoes.
    /// * **fanout** — `fanoutRecord.c:103`, at the top of the same routine that
    ///   runs the SELM switch, which is what the dispatch is; unconditional,
    ///   so SELN refreshes whatever SELM says.
    /// * **seq** — `seqRecord.c:152`, INSIDE the `else` of `if (prec->selm ==
    ///   seqSELM_All)`, so an All-mode seq never reads SELL: SELN stays frozen
    ///   and its `if (seln != oldn)` monitor is a no-op. seq is also async, and
    ///   its `if (prec->pact) return asyncFinish(prec)` (`:141`) means the read
    ///   happens on the first pass only — which the output dispatch already
    ///   gives it.
    ///
    /// `sseq` reads its own SELL→SELN in `SseqRecord::pre_input_link_actions`
    /// (the async machine owns the whole cycle), so it is not handled here.
    pub(crate) fn read_sell_into_seln(
        &self,
        rec: InFrame<'_>,
        phase: SellPhase,
        visited: &mut ProcStack,
    ) {
        let parsed = {
            let instance = rec.read();
            let record_type = instance.record.record_type();
            let reads = match (record_type, phase) {
                ("dfanout", SellPhase::BeforeAlarms) | ("fanout", SellPhase::OutputDispatch) => {
                    true
                }
                // seqSELM_All == 0 (seqRecord.dbd menu(seqSELM) order
                // All/Specified/Mask).
                ("seq", SellPhase::OutputDispatch) => Self::field_i16(&instance, "SELM") != 0,
                _ => false,
            };
            if !reads {
                return;
            }
            // C reads SELL via `dbGetLink` for any link type; the pre-fix port
            // only read a `ParsedLink::Db` SELL, so a SELL sourced over CA/PVA
            // or given as a constant never updated SELN.
            let parsed = instance.record_link(SELL[0]);
            if matches!(**parsed, crate::server::record::ParsedLink::None) {
                return;
            }
            parsed.clone()
        };
        // Through the one classifier: a CONSTANT SELL delivers NOTHING here
        // (`dbConstGetValue`) — its value reached SELN once, at init
        // (`fanoutRecord.c:88`, `dfanoutRecord.c:102`, `seqRecord.c:121`
        // `recGblInitConstantLink(&sell, DBF_USHORT, &seln)`), so a
        // `caput REC.SELN` STAYS put.
        // fanout, dfanout and seq all read SELL at `DBR_USHORT`, so the read
        // delivers SELN's own type, converted as C's GET table converts it.
        if let Some(seln) = self.db_get_link_in(rec, "SELL", &parsed, visited).value() {
            let mut instance = rec.write();
            let _ = instance.record.put_field("SELN", seln);
        }
    }

    /// Multi-output dispatch for fanout, dfanout, seq record types.
    ///
    /// The per-record payload is a typed [`MultiOut`] — seq / sseq
    /// groups are kept as struct fields, NOT `\0`-packed strings
    /// (the pre-fix encoding could mis-split a link string that
    /// happened to contain an embedded NUL).
    ///
    /// All three run inside C's `process()`, before the `recGblResetAlarms`
    /// that commits the cycle — fanout's `dbScanFwdLink`s included
    /// (`fanoutRecord.c:103-146`) — so this is called from the output stage
    /// alone, and returns the pending alarm `(stat, sevr)` the record raises
    /// alongside its links — a failed `dbPutLink` (dfanout's own
    /// `LINK_ALARM/MAJOR`, `dfanoutRecord.c:311-312`) or a `SELN` out of range
    /// (`SOFT_ALARM/INVALID`, `fanoutRecord.c:116`, `dfanoutRecord.c:317`,
    /// `seqRecord.c:157`) — for the caller to fold into `nsev` before that
    /// commit.
    ///
    /// `skip_out` is the IVOA=Don't_drive veto as decided by the cycle's single
    /// IVOA owner (`process_record_with_links_inner`) on the severity
    /// `checkAlarms` produced: C's `if (prec->nsev < INVALID_ALARM)
    /// push_values(); else switch (ivoa)` (`dfanoutRecord.c:128`). Only
    /// dfanout reads it; fanout and seq have no IVOA.
    /// `name` is the record's canonical name, the frame's.
    pub(crate) fn dispatch_multi_output(
        &self,
        name: &[u8],
        rec: InFrame<'_>,
        skip_out: bool,
        visited: &mut ProcStack,
    ) -> MultiOutDispatch {
        // Snapshot the source record's PUTF bit + put-notify wait-set so
        // every write_db_link_value_in call below propagates them to its
        // target — C `dbDbLink.c::processTarget` PUTF and `dbNotifyAdd`
        // wait-set invariants (see write_db_link_value_in doc).
        let (src_putf, src_notify) = {
            let guard = rec.read();
            (guard.common.putf, guard.notify.clone())
        };
        // One snapshot threaded to every OUT-link write below; each arm
        // overrides `field` with the link field it is driving (OUTn / LNKn),
        // which C `setLinkAlarm` reports in the record's AMSG.
        let out_src = OutLinkSrc {
            putf: src_putf,
            notify: OutNotify::chain(src_notify.as_ref()),
            field: "",
        };

        self.read_sell_into_seln(rec, SellPhase::OutputDispatch, visited);

        let dispatch_info: Option<(SelmResult, MultiOut, Option<EpicsValue>)> = {
            let instance = rec.read();
            match instance.record.record_type() {
                "fanout" => {
                    let selm = Self::field_i16(&instance, "SELM");
                    let seln = Self::field_u16(&instance, "SELN");
                    let offs = Self::field_i16(&instance, "OFFS");
                    let shft = Self::field_i16(&instance, "SHFT");
                    // C parity (fanoutRecord.c:39): 16 forward links
                    // LNK0..LNKF. LNK0 is the natural first slot.
                    // SELM resolution with OFFS/SHFT bias (fanoutRecord.c).
                    let sel = select_link_indices_ex(SelmKind::FanoutSeq, selm, seln, offs, shft);
                    Some((sel, MultiOut::Fanout, None))
                }
                "dfanout" => {
                    let selm = Self::field_i16(&instance, "SELM");
                    let seln = Self::field_u16(&instance, "SELN");
                    // C `push_values` pushes `prec->val` — nothing else
                    // (`dfanoutRecord.c:311/323/332`: `dbPutLink(plink,
                    // DBR_DOUBLE, &prec->val, 1)`). IVOA is not re-decided
                    // here: the cycle's IVOA owner already ran
                    //
                    //     case menuIvoaSet_output_to_IVOV:
                    //         prec->val = prec->ivov;      /* :137 */
                    //         push_values(prec);
                    //
                    // through `Record::apply_invalid_output_value`, so VAL
                    // already IS IVOV when that arm was taken — and VAL, not a
                    // second read of IVOV, is what the record posts to its own
                    // monitors. `skip_out` is the Don't_drive veto (`:139`,
                    // `break` — no push) from that same decision.
                    //
                    // Reading `nsev` here instead would re-derive the decision
                    // AFTER the record's other outputs ran, off an alarm they
                    // may have raised themselves.
                    let val = if skip_out {
                        None
                    } else {
                        instance.record.val()
                    };
                    // dfanout Specified is 1-based; Mask has no SHFT
                    // (dfanoutRecord.c:308-339).
                    let sel = select_link_indices_ex(SelmKind::Dfanout, selm, seln, 0, 0);
                    let links = selected_links(&instance, sel);
                    Some((sel, MultiOut::Dfanout(links), val))
                }
                "seq" => {
                    let selm = Self::field_i16(&instance, "SELM");
                    let seln = Self::field_u16(&instance, "SELN");
                    let offs = Self::field_i16(&instance, "OFFS");
                    let shft = Self::field_i16(&instance, "SHFT");
                    // C parity (seqRecord.c:86): 16 link groups 0..F,
                    // each DOLn / DOn (value storage) / DLYn / LNKn.
                    let dly_names = [
                        "DLY0", "DLY1", "DLY2", "DLY3", "DLY4", "DLY5", "DLY6", "DLY7", "DLY8",
                        "DLY9", "DLYA", "DLYB", "DLYC", "DLYD", "DLYE", "DLYF",
                    ];
                    let sel = select_link_indices_ex(SelmKind::FanoutSeq, selm, seln, offs, shft);
                    let by_name = |field| {
                        instance
                            .record
                            .get_field(field)
                            .and_then(|v| v.to_f64())
                            .unwrap_or(0.0)
                    };
                    let groups: Vec<SeqGroup> = sel
                        .indices()
                        .map(|i| {
                            let (dly, dov) =
                                instance.record.seq_group_cells(i).unwrap_or_else(|| {
                                    (by_name(dly_names[i]), by_name(SEQ_DO_FIELDS[i]))
                                });
                            SeqGroup {
                                idx: i,
                                dol: instance.dol_row_link(i).clone(),
                                lnk: instance.out_row_link(i).clone(),
                                dly,
                                dov,
                            }
                        })
                        .collect();
                    Some((sel, MultiOut::Seq(groups), None))
                }
                _ => None,
            }
        };

        let (sel, payload, val) = match dispatch_info {
            Some(info) => info,
            None => return MultiOutDispatch::default(),
        };
        debug_assert!(sel.indices().all(|i| i < payload.len()));
        // Single-owner invariant: every record type that produces a
        // `MultiOut` payload here MUST be listed in
        // `multi_output_dispatch_owned` so the generic
        // `multi_output_links` block in `processing.rs` skips it. If
        // this fires, the two lists have diverged and the skipped
        // type would be dispatched twice per cycle.
        debug_assert!(multi_output_dispatch_owned(rec.read().record.record_type()));

        // C raises SOFT_ALARM/INVALID_ALARM when SELN/OFFS/SHFT resolve
        // out of range (fanoutRecord.c:116, dfanoutRecord.c:317,
        // seqRecord.c:157), folded into the pending nsev the caller commits
        // THIS cycle — returned below, not raised now (a direct SEVR raise
        // would be clobbered by the caller's `recGblResetAlarms`).
        let pending_selm_alarm = sel.alarm;
        let indices = sel.indices();
        // C `dfanoutRecord.c` push_values raises LINK_ALARM/MAJOR per failed
        // dbPutLink; accumulated across the selected OUT links below.
        let mut link_failed = false;

        match payload {
            MultiOut::Fanout => {
                for idx in indices {
                    use crate::server::record::ParsedLink;
                    // A DB target's processing may put this record's LNKn, so
                    // only its bound target leaves the borrow (unbound only
                    // before `iocInit`); an external one is dispatched under
                    // it; any other kind has no `scanForward` and C's
                    // `dbScanFwdLink` does nothing.
                    let db = match &**rec.read().out_row_link(idx) {
                        ParsedLink::Db(db) => Some(db.forward()),
                        link if link.external_forward().is_some() => None,
                        _ => continue,
                    };
                    match db {
                        Some(None) => {}
                        Some(Some(target)) => {
                            // C `fanoutRecord.c:110/121/138` dispatches each
                            // selected LNKn via `dbScanFwdLink` →
                            // `dbDbScanFwdLink` → `dbScanPassive`
                            // (`dbDbLink.c:425-432`), which processes the
                            // target ONLY when its SCAN is Passive
                            // (`if (pto->scan != 0) return 0;`). A LNKn
                            // pointing at a Periodic/Event/I/O-Intr record
                            // must NOT be re-processed by the fanout — that
                            // record runs on its own scan. `dbScanPassive`
                            // then calls `processTarget`, which propagates
                            // PUTF (and sets RPRO on a busy target) exactly
                            // like the explicit FLNK path — so this goes
                            // through the same single owner.
                            // `dbScanFwdLink` returns nothing to the fanout.
                            if let Some(target) = self.record_at(target) {
                                let src = TargetSrc {
                                    rec: Some(rec.unframed()),
                                    putf: src_putf,
                                    notify: src_notify.as_ref(),
                                };
                                let _ = self.process_target_at(
                                    src,
                                    target,
                                    ProcessTargetGate::ScanPassive,
                                    None,
                                    visited,
                                );
                            }
                        }
                        // An external link goes to its lset's `scanForward`
                        // (`dbLink.c:475-481`), the FLNK's own owner.
                        None => self
                            .scan_forward_external(rec, super::processing::ForwardLink::Row(idx)),
                    }
                }
            }
            MultiOut::Dfanout(mut links) => {
                if let Some(val) = val {
                    for idx in indices {
                        // Taken, so the handle ends inside the put.
                        let Some(parsed) = links[idx].take() else {
                            continue;
                        };
                        if matches!(*parsed, crate::server::record::ParsedLink::None) {
                            continue;
                        }
                        // C `dfanoutRecord.c:323` drives each OUTn via
                        // `dbPutLink` → `dbDbPutValue`: an `DBF_OUTLINK`
                        // target is processed only when the link carries
                        // an explicit ` PP` token or the destination is
                        // `.PROC` (`dbDbLink.c:387-390`). A modifier-less
                        // OUTn is NPP — the value is written but the
                        // target is NOT re-processed (otherwise a
                        // Soft-Channel ai target's `convert()` would
                        // clobber the value just written). The NPP
                        // default and the `.PROC`/`PP` processing rule
                        // are both honoured by `parse_output_link_v2`
                        // (uniform NoProcess default) +
                        // `write_db_link_value_in` (write-path target
                        // processing), so no per-call downgrade is needed.
                        // Through the put owner, so a failed OUTn raises the
                        // record's LINK_ALARM/INVALID from inside the put (C
                        // `dbPutLink`'s `setLinkAlarm`) — an external
                        // `ca://`/`pva://` OUTn goes through the link set's
                        // `putValue` exactly as a DB link does
                        // (dbLink.c:434-448). `link_failed` additionally
                        // carries dfanout's OWN LINK_ALARM/MAJOR
                        // (`dfanoutRecord.c:311-312`), which C raises on top of
                        // (and is subsumed by) the put's INVALID.
                        if self
                            .write_out_link_value(
                                rec,
                                parsed,
                                super::PutSrc::Value(&val),
                                OutLinkSrc {
                                    field: DFANOUT_LINK_FIELDS[idx],
                                    ..out_src
                                },
                                visited,
                            )
                            .is_err()
                        {
                            link_failed = true;
                        }
                    }
                    // VAL is a DBF_DOUBLE: nothing to free.
                    val.release();
                }
            }
            MultiOut::Seq(groups) => {
                // C `dbLinkIsConstant` is true for empty and numeric-literal
                // links; a real link is a DB/CA/PVA PV reference.
                // C `seqRecord.c:182-189`: a group is processed iff its LNKn
                // OR DOLn is a real (non-constant) link. When both are
                // constant/empty the group is skipped entirely — so a DOL-only
                // group (real DOLn, empty LNKn) still reads back DOn and posts
                // it, even though nothing is driven. C builds this list into
                // `pcb->grps[]` inside `process()` (`seqRecord.c:182-193`); the
                // callback chain then walks it by `pcb->index`.
                let mut active = groups;
                active.retain(|g| Self::seq_link_is_real(&g.lnk) || Self::seq_link_is_real(&g.dol));
                // C `seqRecord.c:189-196`: an empty group list is the ONLY
                // synchronous exit — `if (!i) return asyncFinish(prec)`. With
                // one or more groups, `process` sets `prec->pact = TRUE`
                // (`:143`) and returns through `processNextLink`, which arms
                // the group on the callback task whatever its DLY: "Always use
                // the callback task to avoid recursion" (`:210-215`), so a
                // zero-delay group is `callbackRequest(&pcb->callback)` and
                // only a positive one is `callbackRequestDelayed(.., dly)`.
                // Each group then runs in `processCallback`, which takes
                // `dbScanLock` itself (`:243-274`), and the exhausted chain
                // re-enters `rset->process` → `asyncFinish` (`:208`,
                // `:219-241`).
                //
                // Ported one-for-one: PACT here, the group walk on a task that
                // re-takes THIS record's L1 gate per group, and
                // `complete_async_record` as `asyncFinish`. `spawn_background`
                // is the callback pool (`callbackRequest`) and
                // `sleep_background` its delayed timer
                // (`callbackRequestDelayed`), so the DLY test belongs inside
                // the walk — where it already is — and not on the decision to
                // go async at all. Which of the three queues that walk runs on
                // is C `seqRecord.c:145-146`, re-read at the top of every
                // `process()` — "Set callback from PRIO",
                // `callbackSetPriority(prec->prio, &pcb->callback)` — so the
                // band is taken here, under the same write lock that sets PACT.
                if !active.is_empty() {
                    let rec_name = name.to_owned();
                    let prio = {
                        let instance = rec.write();
                        instance.enter_pact();
                        instance.common.callback_priority()
                    };
                    let db = self.clone();
                    let rec = Arc::clone(rec.unframed());
                    crate::runtime::task::spawn_background(prio, async move {
                        for grp in &active {
                            let dly = grp.dly;
                            if dly > 0.0 {
                                crate::runtime::task::sleep_background(
                                    crate::runtime::time::duration_from_secs(dly),
                                )
                                .await;
                            }
                            {
                                // C `processCallback`'s `dbScanLock` /
                                // `dbScanUnlock` pair (`seqRecord.c:252`,
                                // `:274`) — held for this group alone. The
                                // seq is claimed as `processTarget`'s source.
                                let mut visited = crate::server::database::ProcStack::new();
                                visited.claim(&rec);
                                db.with_frame(&rec, |frame| {
                                    db.seq_group_step(frame, grp, &mut visited)
                                });
                            }
                        }
                        // `pgrp == NULL` → `prec->rset->process(prec)`
                        // (`seqRecord.c:206-209`), which takes the `pact`
                        // branch into `asyncFinish`.
                        let _ = db.complete_async_record(&rec_name).await;
                    });
                    return MultiOutDispatch::went_async();
                }
                // `active` is empty: C's `if (!i) return asyncFinish(prec)`
                // (`seqRecord.c:189-190`). Nothing to walk, and the caller's
                // own epilogue is `asyncFinish` — so fall through with PACT
                // never set, exactly as C returns without arming a callback.
            }
        }

        // The pending alarm the C record raises alongside its links, for the
        // caller to fold into `nsev` before `recGblResetAlarms`. C raises at
        // most one: SOFT_ALARM/INVALID
        // for a `seln` out of range (dfanoutRecord.c:317, seqRecord.c:157 — no
        // push at all) OR dfanout's own LINK_ALARM/MAJOR for a failed
        // dbPutLink (dfanoutRecord.c:312/324/333). They are mutually exclusive
        // in C's control flow; if both were somehow set, fold the higher
        // severity (recGblSetSevr is raise-only). `link_failed` is dfanout-only
        // — seq's failed LNKn put raises LINK_ALARM/INVALID from inside the put
        // owner (`write_out_link_value`), into the same pending alarm, and
        // seqRecord.c adds nothing on top of it.
        let link_alarm = if link_failed {
            Some((
                crate::server::recgbl::alarm_status::LINK_ALARM,
                AlarmSeverity::Major,
            ))
        } else {
            None
        };
        MultiOutDispatch {
            alarm: match (pending_selm_alarm, link_alarm) {
                (Some(a), Some(b)) => Some(if (a.1 as u16) >= (b.1 as u16) { a } else { b }),
                (a, b) => a.or(b),
            },
            went_async: false,
        }
    }

    /// C `dbLinkIsConstant` negated — a real link is a DB/CA/PVA PV
    /// reference, not an empty or numeric-literal field.
    fn seq_link_is_real(link: &crate::server::record::ParsedLink) -> bool {
        !matches!(
            link,
            crate::server::record::ParsedLink::None
                | crate::server::record::ParsedLink::Constant(_)
        )
    }

    /// One `seq` link group — C `processCallback`'s body
    /// (`seqRecord.c:243-274`), minus the `dbScanLock`/`dbScanUnlock` pair the
    /// caller owns.
    ///
    /// The two callers differ only in who holds the record's gate: the
    /// zero-delay walk runs inside the processing cycle that already holds it,
    /// the delayed chain re-takes it per group. Both read the record's PENDING
    /// alarm HERE rather than once per dispatch, because C's `dbPutLink` reads
    /// `psrce->nsta/nsev` at put time (`dbDbLink.c:382-383`) — so a group whose
    /// LNKn put failed propagates that LINK_ALARM into the NEXT group's target.
    fn seq_group_step(&self, rec: InFrame<'_>, grp: &SeqGroup, visited: &mut ProcStack) {
        let idx = grp.idx;
        let lnk_real = Self::seq_link_is_real(&grp.lnk);
        let dol_real = Self::seq_link_is_real(&grp.dol);
        // C `seqRecord.c:259` `dbGetLink(&dol, DBR_DOUBLE, &dov)`: read DOLn
        // into the DOn value field. A constant/empty DOL — or a failed read —
        // leaves DOn at its previous value. Through the read owner, so a dead
        // DOLn raises `setLinkAlarm` (LINK/INVALID, AMSG `field DOL0`) on the
        // seq itself the way every other `dbGetLink` in the port does.
        let new_dov = if dol_real {
            self.db_get_link_in(rec, DOL_LINK_FIELDS[idx], &grp.dol, visited)
                .value()
                .and_then(|v| v.to_f64())
                .unwrap_or(grp.dov)
        } else {
            grp.dov
        };
        // C `seqRecord.c:261` `recGblGetTimeStamp(prec)`, between the DOLn
        // read and the LNKn put: `seq` restamps once PER GROUP, so a group
        // that waited out its `DLYn` drives its target with the time of ITS
        // hop and not the cycle's. `process` (`seqRecord.c:133-148`) stamps
        // nothing at all; the only other call is `asyncFinish`'s (`:224`),
        // which lands the last hop's time on the record. Through the owner,
        // so TSEL is re-resolved per group exactly as C's re-entry does.
        self.rec_gbl_get_time_stamp_in(rec, visited);
        // C `seqRecord.c:264` drives LNKn via `dbPutLink` (`DBR_DOUBLE,
        // &dov`), whose `DBF_OUTLINK` target is processed by `dbDbPutValue`
        // (`dbDbLink.c:388`) only when the link carries an explicit `PP`
        // modifier. A bare `LNKn` is NPP — the value is written but the target
        // is NOT processed. `parse_output_link_v2` applies that NPP default
        // (the dfanout arm open-codes the same downgrade). LNKn may be a local
        // DB link or an external `ca://`/`pva://` link — C `dbPutLink` routes
        // both through the link set's `putValue` (dbLink.c:434-448). Only a
        // real LNK does anything; a constant LNK is a no-op.
        if lnk_real {
            let (src_putf, src_notify) = {
                let guard = rec.read();
                (guard.common.putf, guard.notify.clone())
            };
            // Through the put owner: a failed LNKn `dbPutLink` raises the seq
            // record's LINK_ALARM/INVALID from inside the put (C
            // `dbLink.c:444-446`). seq's `process()` returns 0.
            let _ = self.write_out_link_value(
                rec,
                Arc::clone(&grp.lnk),
                super::PutSrc::Owned(EpicsValue::Double(new_dov)),
                OutLinkSrc {
                    putf: src_putf,
                    notify: OutNotify::chain(src_notify.as_ref()),
                    field: LNK_LINK_FIELDS[idx],
                },
                visited,
            );
        }
        // C `seqRecord.c:266-268`: store DOn and post a DBE_VALUE|DBE_LOG
        // monitor only when it changed against `odov`, the value this group
        // started with. The field already holds that value, so an unchanged
        // DOn is neither written nor posted.
        if new_dov != grp.dov {
            let field = SEQ_DO_FIELDS[idx];
            let mut inst = rec.write();
            if inst
                .put_internal_field(field, EpicsValue::Double(new_dov))
                .is_ok()
            {
                inst.notify_field(
                    field,
                    crate::server::recgbl::EventMask::VALUE | crate::server::recgbl::EventMask::LOG,
                );
            }
        }
    }

    /// Post the software event named by an `event` record's `VAL`.
    ///
    /// Mirrors C `eventRecord.c:120` `postEvent(prec->epvt)` — every
    /// `process()` of an event record posts its event, waking the
    /// `SCAN="Event"` records whose `EVNT` resolves to that name.
    /// No-op for any other record type, or when `VAL` is empty /
    /// resolves to event 0 (`eventNameToHandle` returns NULL).
    pub(crate) fn dispatch_event_record(&self, rec: InFrame<'_>) {
        let Some(event) = rec.read().record.software_event().cloned() else {
            return;
        };
        // C `postEvent` queues callbacks on the scan ring buffer —
        // the event-scanned records run on a separate callback thread,
        // NOT recursively inside this process cycle. Spawn the routed
        // post so a chain of event records cannot recurse unboundedly
        // and the current cycle's FLNK/CP dispatch is not blocked.
        let db = self.clone();
        // Middle band, not any record's PRIO: C `postEvent` fires one
        // `callbackRequest` per non-empty band, each carrying the *scanned*
        // record's priority (`dbScan.c:513-527`), and the port keeps one Event
        // list rather than three (`scan_index.rs` `post_event_named`), so
        // there is no per-band fan-out to name here.
        crate::runtime::task::spawn_background(
            crate::runtime::task::CallbackPriority::Medium,
            async move {
                db.post_event_named(event.name()).await;
            },
        );
    }

    fn canonical_name(&self, name: impl AsRef<[u8]>) -> RecordName {
        let name = name.as_ref();
        self.resolve_alias(name)
            .unwrap_or_else(|| RecordName::from(name))
    }

    /// Get holder edges to process when the remote PV `external_pv` changes
    /// (external CA/PVA CP/CPP links).
    pub fn get_external_cp_targets(&self, external_pv: &[u8]) -> Vec<super::CpTarget> {
        self.inner
            .external_cp_links
            .load()
            .targets(external_pv)
            .cloned()
            .unwrap_or_default()
    }

    /// Every external PV name that has at least one CP/CPP holder edge.
    /// The calink/pvalink integration warms (opens a monitor for) each of
    /// these at iocInit so the remote source has a live subscription whose
    /// callback can drive an [`super::ExternalCpProcess`] — a Passive
    /// CP holder is otherwise never read and its monitor never opens.
    pub async fn external_cp_pv_names(&self) -> Vec<RecordName> {
        self.inner
            .external_cp_links
            .load()
            .sources()
            .cloned()
            .collect()
    }

    /// Every DISTINCT external PV name this database's link fields name, in
    /// any direction and under any policy — the names the link registry's
    /// links carry once iocInit's staged opens have all landed.
    ///
    /// Distinct from [`Self::external_cp_pv_names`], which is the CP/CPP
    /// subset, and from the counts [`Self::setup_cp_links`] and
    /// [`Self::setup_external_link_opens`] print: those count *link fields*
    /// (two records pointing at one upstream PV are two links), whereas this
    /// names each PV once.
    ///
    /// The enumeration goes through [`super::PvDatabase::record_link_fields`]
    /// and `external_pv_name`, the same two owners both iocInit link phases
    /// use, so "what is an external link" cannot diverge between staging a
    /// link and counting it.
    ///
    /// Sorted, so a caller can print it as a stable operator-facing list.
    pub async fn external_link_pv_names(&self) -> Vec<RecordName> {
        let mut seen: Vec<RecordName> = Vec::new();
        for record_name in &self.all_record_names() {
            for (_field, _raw, parsed) in self.record_link_fields(record_name) {
                let Some(pv) = parsed.external_pv_name() else {
                    continue;
                };
                if !seen.iter().any(|s| *s == *pv) {
                    seen.push(pv.as_ref().into());
                }
            }
        }
        seen.sort();
        seen
    }

    /// C `dbInitLink`'s locality decision for ONE parsed link — the single
    /// owner of "a DB link whose target is not in this IOC is a CA link".
    ///
    /// C chain. `dbInitLink` hands a modifier-less `PV_LINK` to
    /// `dbDbInitLink` and returns only if it succeeds
    /// (`dbLink.c:118-121`: `if (!dbDbInitLink(plink, dbfType)) return;`).
    /// `dbDbInitLink` calls `dbChannelCreate(pvname)` and bails with
    /// `S_db_notFound` when no such record exists in this IOC
    /// (`dbDbLink.c:94-96`). Execution therefore falls through to
    /// `dbCaAddLink` (`dbLink.c:128`) and the link becomes a CA link. (Line
    /// numbers in this paragraph are the `R7.0.10` pin; this machine's
    /// checkout `R7.0.10-146-g8f5015b66` replaces that call with
    /// `dbCaAddLinkCallbackOpt` at `:130` for the unreleased iocInit
    /// connection-wait — see `PvDatabase::external_link_targets`.)
    /// A link that already carries `CA`/`CP`/`CPP` skips
    /// `dbDbInitLink` entirely (`dbLink.c:118`) and reaches the very same
    /// call — which is why this rule is policy-agnostic and
    /// direction-agnostic: it is the one locality decision, not a CP-only
    /// one. Restricting it to CP/CPP left every plain non-local `Db` link
    /// unconverted and unopened.
    ///
    /// **Decided once.** C sets `DBLINK_FLAG_INITIALIZED` on entry and
    /// returns early on every later call (`dbLink.c:96-100`), so a record
    /// added to the IOC *after* iocInit does NOT un-convert a link that was
    /// already made external. [`Self::initialize_link_locality`] reproduces
    /// that by committing the decision into the holder's parsed-link cache,
    /// which is never re-derived from locality afterwards.
    ///
    /// Every other variant is returned unchanged: `Constant`
    /// (`dbConstInitLink`, `dbLink.c:102-105`), the JSON links
    /// (`dbJLinkInit`, `:107-110`), a hardware link and an already-external
    /// `Ca`/`Pva` link all take a different arm of `dbInitLink` and never
    /// consult record locality.
    pub(crate) fn db_init_link_locality(
        &self,
        parsed: crate::server::record::ParsedLink,
        ftype: crate::server::record::LinkFieldType,
    ) -> crate::server::record::ParsedLink {
        // "Parsed" must not be able to mean "serviceable". A link naming a
        // scheme whose [`LinkSet`](super::LinkSet) was never installed is
        // refused here and becomes the empty link C is left with when
        // `dbjl_map_key` finds no `link()` entry for the type and
        // `dbLoadRecords` rejects the field (`dbJLink.c:262-266`) — measured
        // on `R7.0.10-146-g8f5015b66`: `INP : CONSTANT`, `STAT: UDF`,
        // `SEVR: INVALID`, `UDF: 1`.
        //
        // It sits in THIS function rather than in the iocInit pass because
        // this is the single owner of the post-`dbInitLink` view: every
        // consumer reaches it, the cached link fields through
        // `initialize_link_locality` and the re-parsed ones through
        // `record_link_fields` (which re-reads the raw text, so a rewrite
        // committed only to the parse cache would leave `setup_cp_links`,
        // `setup_external_link_opens` and `dbcaxr` still servicing the
        // refused link). The diagnostic is printed once, by
        // `initialize_link_locality`; this classification is silent because
        // `record_link_fields` runs it on every call.
        self.relink(&parsed, ftype).unwrap_or(parsed)
    }

    /// [`Self::db_init_link_locality`]'s rule, `None` when the link stays as
    /// parsed. Also C `dbAddLink` (`dbLink.c:146-191`) for a runtime put of
    /// a link field ([`PreparedLink::relink`](crate::server::record::PreparedLink::relink)),
    /// which decides locality the same way and sets `pvlOptFWD` silently.
    pub(crate) fn relink(
        &self,
        parsed: &crate::server::record::ParsedLink,
        ftype: crate::server::record::LinkFieldType,
    ) -> Option<crate::server::record::ParsedLink> {
        if let Some(scheme) = parsed.required_link_set_scheme()
            && self.inner.link_sets.load().get(scheme).is_none()
        {
            return Some(crate::server::record::ParsedLink::None);
        }
        let crate::server::record::ParsedLink::Db(db) = parsed else {
            return None;
        };
        if self.has_db_link_target(db.target().record.as_bytes(), db.field_as_written())
            && Self::db_link_filter_refusal(db).is_none()
        {
            return None;
        }
        Some(crate::server::record::ParsedLink::Ca(
            crate::server::record::CaLink {
                // `DbLink::channel_name` is the same string C keeps in
                // `pv_link.pvname` across the `dbChannelCreate` → `dbCaAddLink`
                // fallthrough, so the DB target and the CA channel cannot drift.
                fwd: crate::server::record::ca_forward_flag(ftype, db.pvname()),
                open: crate::server::record::OpenLatch::ca(db.policy, ftype),
                pv: db.pvname().clone(),
                monitor_switch: db.monitor_switch,
                policy: db.policy,
            },
        ))
    }

    /// Why `dbDbInitLink`/`dbDbAddLink`'s `dbChannelCreate` refuses a local
    /// DB link's filter chain, sending it on to `dbCaAddLink`
    /// (`dbDbLink.c:89-102`, `dbLink.c:118-130`).
    fn db_link_filter_refusal(
        db: &crate::server::record::DbLink,
    ) -> Option<crate::server::database::filters::ChannelCreateError> {
        let suffix = db.target().json_suffix.as_deref()?;
        crate::server::database::filters::try_parse_filter_chain(suffix)
            .err()
            .map(crate::server::database::filters::ChannelCreateError::Filter)
    }

    /// C `dbAddLink` (`dbLink.c:146-191`) for a link put: [`Self::relink`],
    /// and a local forward that stays as parsed bound ([`Self::bind_forward`]).
    /// An external one gets its handle from the put's open
    /// ([`Self::open_put_link`]).
    pub(crate) fn add_link(
        &self,
        parsed: &crate::server::record::ParsedLink,
        ftype: crate::server::record::LinkFieldType,
    ) -> Option<crate::server::record::ParsedLink> {
        let next = self.relink(parsed, ftype);
        if next.is_none() && ftype == crate::server::record::LinkFieldType::Fwd {
            self.bind_forward(parsed, ftype);
        }
        next
    }

    /// The lset's `scanForward` handle stored in an external forward link, so
    /// a dispatch only runs it. The one writer of a
    /// [`ForwardSlot`](crate::server::record::ForwardSlot), called only for a
    /// link its lset just opened ([`Self::bind_opened`]): one it opens with no
    /// lset is `Removed` and forwards nothing (`dbLink.c:479`).
    fn bind_external_forward(&self, parsed: &crate::server::record::ParsedLink) {
        use crate::server::record::ParsedLink;
        let (key, slot) = match parsed {
            ParsedLink::Pva(p) => (p.name(), &p.forward),
            ParsedLink::PvaJson(j) => (j.link_identity_key(), &j.forward),
            _ => return,
        };
        let (Some(scheme), Some(external)) = (parsed.required_link_set_scheme(), parsed.external())
        else {
            return;
        };
        let open = external.open;
        if let Some(lset) = self.inner.link_sets.load().get(scheme) {
            slot.bind(|| lset.forward_handle(super::LinkRef::new(key.as_bytes(), open)));
        }
    }

    /// The open of every external forward link no lset was there to open, when
    /// a link set registers after `iocInit`. Before it, the init open
    /// ([`Self::open_record_links`]) is still to come and opens them itself.
    pub(super) fn open_unopened_forwards(&self) {
        use crate::server::record::ExternalForward;
        if !self.ioc_is_running() {
            return;
        }
        for (_, rec) in self.iterate_records() {
            for (field, link) in forward_links(&rec) {
                let unopened = matches!(
                    link.external_forward(),
                    Some(ExternalForward::Pva(slot)) if !slot.is_bound()
                );
                if !unopened {
                    continue;
                }
                let holder =
                    super::LinkHolder::new(rec.rdes().record_type, rec.name().as_bytes(), field);
                if let Some(open) = self.open_external_link(holder, &link) {
                    self.bind_opened(&mut rec.write(), field, open);
                }
            }
        }
    }

    /// C `dbInitLink` for every forward link of every record, once the
    /// `iocInit` barrier is crossed ([`Self::init_record_forwards`]).
    pub(super) fn init_forward_links(&self) {
        // C's `iterateRecords` order, which is its warnings' order.
        for (_, rec) in self.iterate_records() {
            self.init_record_forwards(&rec);
        }
    }

    /// C `dbInitLink` for `rec`'s forward links: a DB link naming a local
    /// record is bound to it, any other is the CA link `dbDbInitLink`'s
    /// failure falls through to (`dbLink.c:118-130`). After it no forward is
    /// left a DB link without a target. A record added after `iocInit` runs
    /// it for its own links only; a link already made CA stays CA and
    /// reaches a later record through CA, as C's would. An external link is
    /// opened by [`Self::open_record_links`].
    pub(super) fn init_record_forwards(&self, rec: &crate::server::record::RecordCell) {
        use crate::server::record::{LinkFieldType, ParsedLink};
        let mut rewrites: Vec<(&'static str, Arc<ParsedLink>)> = Vec::new();
        for (field, link) in forward_links(rec) {
            if !matches!(*link, ParsedLink::Db(_)) {
                continue;
            }
            let Some(next) = self.relink(&link, LinkFieldType::Fwd) else {
                self.bind_forward(&link, LinkFieldType::Fwd);
                continue;
            };
            if let ParsedLink::Ca(ca) = &next {
                warn_ca_forward_without_proc(rec.name(), field, ca);
            }
            rewrites.push((field, Arc::new(next)));
        }
        if rewrites.is_empty() {
            return;
        }
        // Parsed above, and the replaced parses dropped after the hold.
        let mut retired = Vec::with_capacity(rewrites.len());
        {
            let mut inst = rec.write();
            for (field, link) in rewrites {
                retired.push(inst.install_link_cache(field, link).unwrap_or_else(|l| l));
            }
        }
        drop(retired);
    }

    /// C `dbDbInitLink`'s `precord` for a local forward link: the target's
    /// record-table index, written into the link so a dispatch reads it and
    /// resolves no name. Only [`Self::init_record_forwards`] and [`Self::add_link`]
    /// call it.
    pub(crate) fn bind_forward(
        &self,
        parsed: &crate::server::record::ParsedLink,
        ftype: crate::server::record::LinkFieldType,
    ) {
        if ftype != crate::server::record::LinkFieldType::Fwd {
            return;
        }
        let crate::server::record::ParsedLink::Db(db) = parsed else {
            return;
        };
        if let Some(index) = self.db_link_addr(db).and_then(|at| at.rec.index()) {
            db.bind_forward(index);
        }
    }

    /// Commit C `dbInitLink`'s locality decision into every record's stored
    /// links — the iocInit pass that makes the rule STATE rather than a test
    /// each read path repeats.
    ///
    /// Runs after all records have loaded and before `setup_cp_links`, which
    /// is C's ordering: `initPVLinks` walks the loaded database once, every
    /// `DBF_INLINK`/`DBF_OUTLINK`/`DBF_FWDLINK` of every record
    /// (`dbLink.c:92-143`). The fields come from the dbd table
    /// (`Self::link_field_texts`), so a fanout `LNKn` or an `INPA` is
    /// decided like `FLNK`.
    ///
    /// The decision is taken from the field's RAW text, not from the stored
    /// parse: a link whose raw string was installed some other way must still
    /// be initialised. Only a genuine `Db` → `Ca` conversion is written back,
    /// so a stored link that legitimately differs from its raw text is left
    /// alone. A CA forward link that will never forward is warned here, in
    /// field order, as `dbInitLink` does (`dbLink.c:137-142`); the runtime
    /// `dbAddLink` sets the flag silently.
    ///
    /// Returns the number of links converted.
    pub async fn initialize_link_locality(&self) -> usize {
        use crate::server::record::{LinkFieldType, ParsedLink};
        let mut converted = 0usize;
        // C's `doResolveLinks` walk (`iocInit.c:484-506`), so its warnings
        // print in softIoc's order.
        for (name, rec) in self.iterate_records() {
            // Decide before taking the record's write lock: the locality query
            // reads the database's record map, and nothing else in this crate
            // holds a record-instance lock across that map.
            let raw = Self::link_field_texts(&rec.read());
            let mut rewrites: Vec<(String, Option<Arc<ParsedLink>>)> = Vec::new();
            for (field, text, ftype) in raw {
                if text.is_empty() {
                    continue;
                }
                let parsed = crate::server::record::parse_link_field(&text, ftype);
                let unserviceable = parsed.required_link_set_scheme();
                let was_db = matches!(parsed, ParsedLink::Db(_));
                let json = matches!(parsed, ParsedLink::PvaJson(_));
                // `dbDbInitLink`'s `dbChannelCreate` line; a put printed its
                // own in `dbPutFieldLink`, which `dbAddLink` reuses.
                if let ParsedLink::Db(db) = &parsed
                    && self.has_db_link_target(db.target().record.as_bytes(), db.field_as_written())
                    && let Some(refusal) = Self::db_link_filter_refusal(db)
                {
                    refusal.report();
                }
                let next = self.db_init_link_locality(parsed, ftype);
                if let ParsedLink::Ca(ca) = &next
                    && ftype == LinkFieldType::Fwd
                {
                    warn_ca_forward_without_proc(&name, &field, ca);
                }
                match (&next, unserviceable) {
                    (ParsedLink::Ca(_), _) if was_db => {
                        rewrites.push((field, Some(Arc::new(next))))
                    }
                    // C names a `{pva:...}` with no `link(pva, ...)` loaded
                    // (`dbJLink.c:264`); a `pva://` text is a CA link to C
                    // (`dbLink.c:119-130`), refused silently.
                    (ParsedLink::None, Some(scheme)) => {
                        if json {
                            crate::runtime::log::errlog_vprintf(format_args!(
                                "dbJLinkInit: Link type '{scheme}' not found\n"
                            ));
                        }
                        rewrites.push((field, Some(Arc::new(next))));
                    }
                    _ => {}
                }
            }
            if !rewrites.is_empty() {
                // The parses were built above and the replaced ones leave in
                // `retired`, reserved here: the hold only moves pointers.
                let mut retired = Vec::with_capacity(rewrites.len());
                {
                    let mut inst = rec.write();
                    // The names stay in `rewrites` until after the hold.
                    for (field, link) in &mut rewrites {
                        let Some(link) = link.take() else { continue };
                        let is_refusal = matches!(*link, ParsedLink::None);
                        match inst.install_link_cache(field, link) {
                            Ok(old) => {
                                converted += usize::from(!is_refusal);
                                retired.push(old);
                            }
                            Err(link) => retired.push(link),
                        }
                    }
                }
                drop((retired, rewrites));
            }
        }
        if converted > 0 {
            tracing::debug!("iocInit: {converted} non-local DB link(s) made external");
        }
        converted
    }

    /// The CP/CPP edge one parsed link makes, if it makes one.
    ///
    /// Only a `Ca` link can: C makes every CP/CPP link a CA link whatever its
    /// target (`dbLink.c:118-122`), and `eventCallback` adds `CA_DBPROCESS`
    /// for each one (`dbCa.c:958-962`). `OTHER CP CA` is not such a link: `CA`
    /// and `CP` are one process class and `CA` is matched first
    /// (`dbStaticLib.c:2369-2373`).
    fn classify_cp_link(parsed: crate::server::record::ParsedLink) -> Option<CpEdge> {
        match parsed {
            crate::server::record::ParsedLink::Ca(ca) => {
                ca.policy.cp_passive_only().map(|passive_only| CpEdge {
                    pv: ca.pv.into(),
                    passive_only,
                    open: ca.open,
                })
            }
            _ => None,
        }
    }

    /// Every CP/CPP link in the database, as `(holder, field, edge)`.
    fn cp_links_of(
        &self,
        holders: impl IntoIterator<Item = RecordName>,
    ) -> Vec<(RecordName, String, CpEdge)> {
        let mut out = Vec::new();
        for holder in holders {
            // Enumerated through the single shared owner (`record_link_fields`)
            // so this and the pvalink install scan never diverge on which
            // fields count as links. A CP/CPP link is `Ca` whatever its
            // target (`dbLink.c:118-127`).
            for (field, _raw, parsed) in self.record_link_fields(&holder) {
                if let Some(edge) = Self::classify_cp_link(parsed) {
                    out.push((holder.clone(), field, edge));
                }
            }
        }
        out
    }

    /// Publish `links`' edges in one registry update, each replacing its
    /// link's previous edge.
    fn set_cp_links<'a>(&self, links: impl IntoIterator<Item = &'a (RecordName, String, CpEdge)>) {
        let edges: Vec<(RecordName, &str, super::CpSource)> = links
            .into_iter()
            .map(|(holder, field, edge)| {
                (
                    self.canonical_name(holder),
                    field.as_str(),
                    Self::cp_source(edge),
                )
            })
            .collect();
        self.inner.external_cp_links.update(|cp| {
            for (holder, field, source) in edges {
                cp.set_link(&holder, field, Some(source));
            }
            cp.mark_links_initialized();
        });
    }

    /// `edge` as the registry keys it: by the PV it names and by its link.
    fn cp_source(edge: &CpEdge) -> super::CpSource {
        super::CpSource {
            record: RecordName::from(external_cp_key(&edge.pv)),
            passive_only: edge.passive_only,
            link: edge.open.serial(),
        }
    }

    /// Scan all records for CP/CPP links, register them, and open each one's
    /// channel. A Passive holder is never scanned, so its link would never
    /// open lazily and the monitor that drives it would never exist; C's
    /// `dbInitLink` → `dbCaAddLink` adds every CA link at init. A no-op
    /// open when no matching link set is installed.
    pub async fn setup_cp_links(&self) {
        let names = self.all_record_names();
        let links = self.cp_links_of(names);
        self.set_cp_links(&links);
        for (_, _, edge) in &links {
            let _ = self.resolve_external_pv(edge.external());
        }
        if !links.is_empty() {
            tracing::debug!("iocInit: {} CP link subscriptions", links.len());
        }
    }

    /// C `dbCaRun` (`iocInit.c:260`, after `scanRun` and the PINI=YES pass):
    /// the CP/CPP processing the links' monitors requested while `dbCaTask`
    /// was paused runs from here, so a holder's connect-time process follows
    /// PINI.
    ///
    /// It first waits for the jobs `scanRun`'s edge started (the asyn ports'
    /// boot flush). A synchronous port's sweep has run by then, so every
    /// interrupt it queued is on its pump before a link's first event takes
    /// its turn there; a `CANBLOCK` port's is only queued behind the port's
    /// requests, and a CP link on its I/O Intr records waits for it in its
    /// own first event, not here.
    pub async fn process_cp_links_on_connect(&self) {
        self.scan_control().interrupt_gate().jobs_settled().await;
        self.inner.ca_run.run().await;
    }

    /// C `dbCaSync`, after `dbCaRun`: return once every event posted to a
    /// local CA link before the call has run its CP/CPP processing.
    pub async fn ca_sync(&self) {
        self.inner.ca_links.sync().await;
    }

    /// `record`'s pump's turn requests (see [`Self::io_intr_turn`]).
    pub(crate) fn register_io_intr_pump(
        &self,
        record: &std::sync::Arc<crate::server::record::RecordCell>,
    ) -> crate::runtime::sync::mpsc::UnboundedReceiver<crate::runtime::sync::oneshot::Sender<()>>
    {
        self.inner.io_intr_turns.register(record)
    }

    /// Return once `record`'s I/O Intr pump has processed every interrupt
    /// queued to it now; at once for a record with no pump.
    pub(crate) async fn io_intr_turn(
        &self,
        record: &std::sync::Arc<crate::server::record::RecordCell>,
    ) {
        self.inner.io_intr_turns.turn(record).await;
    }

    /// A put to `holder`'s link `field`: C `dbPutFieldLink` drops the old
    /// `caLink` ([`LinkPutQueue::remove_link`](super::link_put_queue::LinkPutQueue::remove_link))
    /// and, once iocInit's link pass has run, opens a new one —
    /// `dbCaAddLink` stages its `CA_CONNECT`, as
    /// [`Self::setup_external_link_opens`] does, and `dbCaRun` services it —
    /// whose first monitor event processes a CP holder. A link the put
    /// removed and never added opens nothing. Called once the put's gate is
    /// down.
    pub(crate) fn relink_cp_link(&self, holder: &[u8], field: &str) {
        let holder = self.canonical_name(holder);
        let rec = self.get_record(&holder);
        if let Some(rec) = &rec {
            let field = rec.canonical_field(field);
            let current = super::LinkInstall::of_field(&rec.read(), &field);
            self.inner
                .link_puts
                .remove_link(rec.index(), &field, &current);
        }
        if !self.inner.external_cp_links.load().links_initialized() {
            return;
        }
        let removed = rec
            .as_ref()
            .is_some_and(|rec| rec.read().link_unadded(field));
        let link = self
            .record_link_fields(&holder)
            .into_iter()
            .find(|(f, _, _)| !removed && f.eq_ignore_ascii_case(field))
            .map(|(_, _, parsed)| parsed);
        if let Some(external) = link.as_ref().and_then(|l| l.external()) {
            self.stage_external_link_open(external);
        }
        let edge = link.and_then(Self::classify_cp_link);
        let field = rec.as_ref().map_or_else(
            || crate::server::record::record_instance::upper_field(field),
            |rec| rec.canonical_field(field),
        );
        let keyed = edge.as_ref().map(Self::cp_source);
        self.inner
            .external_cp_links
            .update(|cp| cp.set_link(&holder, &field, keyed));
        // The open staged above may have run its first pass already.
        if let Some(edge) = &edge {
            let (scheme, link) = Self::external_link_ref(edge.external());
            super::link_put_queue::find_lset(&self.inner, scheme, |lset| {
                lset.join_cp_link(link);
                Some(())
            });
        }
    }

    /// Open every external link in the database at iocInit — C
    /// `dbInitLink` (`dbLink.c:92-143`) hands EVERY non-local `PV_LINK` to
    /// `dbCaAddLink` (`dbCa.c:397-401`), which stages a `CA_CONNECT` action
    /// — the `addAction(pca, CA_CONNECT)` at `dbCa.c:393` inside
    /// `dbCaAddLinkCallback` (`:373-395`) — for the `dbCaTask` to service.
    /// A C IOC therefore reaches its first scan with every external channel
    /// already connecting.
    ///
    /// The checkout `R7.0.10-146-g8f5015b66` routes the same staging through
    /// `dbCaAddLinkCallbackOpt`, which arrived with the iocInit
    /// connection-wait (`717d69e1f`, post-`R7.0.10` and in no tag) and does
    /// not exist at the pin. The staging itself is identical at both — see
    /// `PvDatabase::external_link_targets` for the one behaviour the wait
    /// does add.
    ///
    /// Two properties come straight from the C:
    ///
    /// * **Direction-agnostic.** `dbInitLink` reaches `dbCaAddLink` for
    ///   `DBF_INLINK`, `DBF_OUTLINK` and `DBF_FWDLINK` alike (`dbLink.c:118`
    ///   onwards; the `dbfType` check below it only sets `pvlOptInpNative` /
    ///   `pvlOptFWD`). An OUT-only external link is opened at init too.
    /// * **Every link field, not just the CP/CPP subset.**
    ///   [`Self::setup_cp_links`] warms only CP/CPP links, because those have
    ///   a chicken-and-egg problem a lazy open cannot solve (a Passive holder
    ///   is never scanned, so its monitor is never created). Every other
    ///   external link would otherwise wait for its first cache miss to stage
    ///   the open — one cold scan cycle per link that C does not spend.
    ///
    /// The field enumeration is [`super::PvDatabase::record_link_fields`], the
    /// same single owner `setup_cp_links` uses, so "what is a link field"
    /// cannot diverge between the two passes. Staging goes through
    /// `PvDatabase::stage_external_link_open`, which latches the link, so
    /// the connect still runs on the link work
    /// owner and never on a record-processing thread; nothing here calls
    /// `LinkSet::connect_link` directly.
    ///
    /// Idempotent against `setup_cp_links`: the queue's open state is
    /// per-`LinkKey` and terminal, and both passes
    /// derive the key from the same boundary name, so a CP link already warmed
    /// above is not staged a second time.
    ///
    /// Returns the number of links this pass staged.
    pub async fn setup_external_link_opens(&self) -> usize {
        let names = self.all_record_names();
        let mut staged = 0usize;
        for record_name in &names {
            for (_field, _raw, parsed) in self.record_link_fields(record_name) {
                // `external_pv_name` is `Some` exactly for the external
                // variants (`Ca` / `Pva` / `PvaJson`) and carries the key the
                // link set is addressed with. A local `Db` link, a
                // `Constant`, a hardware link and a `lnkCalc` link all report
                // `None` — C's `dbDbInitLink` / `dbConstInitLink` /
                // `dbJLinkInit` paths, none of which reach `dbCaAddLink`.
                let Some(external) = parsed.external() else {
                    continue;
                };
                if self.stage_external_link_open(external) {
                    staged += 1;
                }
            }
        }
        if staged > 0 {
            tracing::debug!("iocInit: {staged} external link opens staged");
        }
        staged
    }
}

/// A `$` address's link read: the view is taken under `instance`'s hold and
/// its `DBF_CHAR` image built after releasing it. `None` is a field `$` does
/// not apply to, which `dbChannelTest` refuses.
#[cold]
#[inline(never)]
fn char_image_after_hold(
    instance: crate::server::record::record_instance::RecordRef<'_>,
    field: &str,
) -> Option<EpicsValue> {
    let view = super::char_view::char_view(&instance, field);
    drop(instance);
    view.map(super::char_view::CharView::into_chars)
}

/// The source alarm a read of `db` reports, taken under the read's hold:
/// only an `MS`/`MSS`/`MSI` link inherits one.
#[inline(always)]
fn db_link_alarm(
    db: &crate::server::record::DbLink,
    at: &crate::server::record::record_instance::ResolvedTarget,
    instance: &crate::server::record::RecordInstance,
) -> Option<SourceAlarm> {
    (db.monitor_switch != crate::server::record::MonitorSwitch::NoMaximize).then(|| {
        SourceAlarm::local(
            SourceId::of(&at.rec),
            LinkAlarm::committed(&instance.common),
        )
    })
}

/// A link read with the source's part of its `DBR_STRING` conversion, when
/// the reader asked for one.
pub(crate) type RenderedRead = (
    Option<EpicsValue>,
    Option<SourceAlarm>,
    Option<crate::server::record::record_instance::SourceRender>,
);

/// The fetch twin of [`RenderedRead`].
pub(crate) type RenderedFetch = (
    crate::server::recgbl::simm::LinkFetch,
    Option<SourceAlarm>,
    Option<crate::server::record::record_instance::SourceRender>,
);

/// The INVALID every failing OUT put owes its record (`setLinkAlarm`).
#[cold]
pub(super) fn set_out_link_alarm(src_rec: InFrame<'_>, field: &'static str) {
    let mut inst = src_rec.write();
    crate::server::recgbl::rec_gbl_set_link_alarm(&mut inst.common, field);
}

/// Whether `at` addresses a `DBF_CHAR`/`DBF_UCHAR` field — C
/// `dbGetLinkDBFtype` of a DB link (`dbDbGetDBFtype`). A runtime-typed field
/// is typed by the variant it holds, as `cvt_dbaddr` is in the port.
#[cold]
fn takes_chars(
    held: Option<&super::SetGuard>,
    rec: &crate::server::record::RecordCell,
    put: &crate::server::record::record_instance::PutAddr,
    field: &str,
) -> bool {
    use crate::types::DbfCode;
    let addr = put.field();
    if addr.char_view() {
        return true;
    }
    let Some(desc) = addr.desc else {
        return false;
    };
    if !desc.runtime_typed {
        let code = super::char_view::field_addr_types(desc, None).0;
        return matches!(code, DbfCode::Char | DbfCode::UChar);
    }
    let guard = match held {
        Some(held) => rec.read_in(held),
        None => rec.read(),
    };
    // The primary field's variant, lent: a waveform VAL is not cloned.
    if rec.rdes().is_primary(field, &*guard.record)
        && let Some(view) = guard.record.monitor_value_view()
    {
        use crate::server::record::ValueView;
        return matches!(view, ValueView::Char(_) | ValueView::UChar(_));
    }
    let value = guard.resolve_field(field);
    let code = super::char_view::field_addr_types(desc, value.as_ref()).0;
    matches!(code, DbfCode::Char | DbfCode::UChar)
}

#[cfg(test)]
mod out_link_put_fail_tests {
    use super::{OutLinkSrc, OutNotify};
    use crate::server::database::PvDatabase;
    use crate::server::record::{AlarmSeverity, DbLink, LinkProcessPolicy, MonitorSwitch};
    use crate::server::records::calc::CalcRecord;
    use crate::types::EpicsValue;

    /// C `dbDbPutValue` (dbDbLink.c:382-390) runs `dbPut`, folds the
    /// source alarm via `recGblInheritSevrMsg`, then `if (status) return
    /// status;` — only a *successful* write reaches the `.PROC`/`PP`
    /// `processTarget` branch. A failed OUT-link write must therefore NOT
    /// process the target. The failure here is a string that names no choice
    /// of a `DBF_MENU` field — C `dbPut` → `putStringMenu` → `S_db_badChoice`.
    ///
    /// Observable: a passive `calc` target with `CALC = "7"` evaluates to
    /// `VAL = 7` on process and stays at its `Default` `VAL = 0` when not
    /// processed. The PP OUT link below carries a write that fails, so the
    /// code returns before `processTarget` and `VAL` stays `0.0`; processing
    /// the target unconditionally would make it `7.0`.
    #[epics_macros_rs::epics_test]
    async fn pp_out_link_failed_write_does_not_process_target() {
        let db = PvDatabase::new();
        db.add_record("TGT", Box::new(CalcRecord::new("7")))
            .await
            .unwrap();

        // Precondition: CALC not yet evaluated, VAL at its Default 0.0.
        assert!(
            matches!(db.get_pv("TGT.VAL").unwrap(), EpicsValue::Double(v) if v == 0.0),
            "calc VAL must start at its Default 0.0 before any process"
        );

        let link = DbLink::new(
            "TGT.PINI",
            LinkProcessPolicy::ProcessPassive, // ` PP` token
            MonitorSwitch::NoMaximize,
        );
        let src = OutLinkSrc {
            putf: false,
            notify: OutNotify::None,
            field: "OUT",
        };
        let mut visited = crate::server::database::ProcStack::new();

        db.write_db_link_value(
            &link,
            EpicsValue::String("NOT_A_MENU_CHOICE".into()),
            src,
            &mut visited,
        );

        // The failed write short-circuits before processTarget, so CALC was
        // never evaluated and VAL is still its Default 0.0.
        assert!(
            matches!(db.get_pv("TGT.VAL").unwrap(), EpicsValue::Double(v) if v == 0.0),
            "a failed OUT-link write must NOT process the PP target \
             (VAL must stay 0.0, not become 7.0)"
        );
    }

    /// R6-7 — an empty array into a scalar field is NOT a failed write. C
    /// `dbPut` (`dbAccess.c:1370-1372`) writes nothing, raises
    /// `LINK_ALARM`/`INVALID_ALARM` on the destination and returns 0, so
    /// `dbDbPutValue`'s `if (status) return status;` does not fire and a ` PP`
    /// link goes on to process its target.
    ///
    /// The port used to reject the put, which suppressed both the destination's
    /// alarm and its `PP` processing.
    #[epics_macros_rs::epics_test]
    async fn pp_out_link_empty_array_alarms_target_and_still_processes_it() {
        use crate::server::recgbl::alarm_status;

        let db = PvDatabase::new();
        db.add_record("ETGT", Box::new(CalcRecord::new("7")))
            .await
            .unwrap();

        let link = DbLink::new(
            "ETGT",
            LinkProcessPolicy::ProcessPassive, // ` PP` token
            MonitorSwitch::NoMaximize,
        );
        let src = OutLinkSrc {
            putf: false,
            notify: OutNotify::None,
            field: "OUT",
        };
        let mut visited = crate::server::database::ProcStack::new();

        db.write_db_link_value(
            &link,
            EpicsValue::DoubleArray(vec![].into()),
            src,
            &mut visited,
        );

        // The put succeeded (status 0), so the PP target processed: CALC = "7".
        assert!(
            matches!(db.get_pv("ETGT.VAL").unwrap(), EpicsValue::Double(v) if v == 7.0),
            "an empty-array put is accepted by C, so the PP target must process"
        );
        // …and the destination carries LINK/INVALID, committed by that very
        // process cycle's `recGblResetAlarms`.
        let inst = db.get_record("ETGT").unwrap();
        let inst = inst.read();
        assert_eq!(inst.common.stat, alarm_status::LINK_ALARM);
        assert_eq!(inst.common.sevr, AlarmSeverity::Invalid);
    }
}

#[cfg(test)]
mod nonlocal_db_link_write_tests {
    use super::{OutLinkSrc, OutNotify};
    use crate::server::database::{LinkPutOp, LinkRef, LinkSet, PvDatabase};
    use crate::server::record::{AlarmSeverity, DbLink, LinkProcessPolicy, MonitorSwitch};
    use crate::server::records::calc::CalcRecord;
    use crate::types::EpicsValue;
    use std::sync::{Arc, Mutex};

    /// A link set that records every `put_value` it receives so a test
    /// can assert a non-local OUT-link write reached the external (CA)
    /// put path instead of being dropped by a local `dbPut`.
    struct RecordingLset {
        puts: Arc<Mutex<Vec<(String, EpicsValue)>>>,
    }
    #[async_trait::async_trait]
    impl LinkSet for RecordingLset {
        fn is_connected(&self, _: LinkRef<'_>) -> bool {
            true
        }
        fn get_cached_value(&self, _: LinkRef<'_>) -> Option<EpicsValue> {
            None
        }
        async fn get_value(&self, link: LinkRef<'_>) -> Option<EpicsValue> {
            self.get_cached_value(link)
        }
        async fn put_value(
            &self,
            link: LinkRef<'_>,
            value: EpicsValue,
            _op: LinkPutOp,
        ) -> Result<(), String> {
            let name = link.name;
            self.puts
                .lock()
                .unwrap()
                .push((String::from_utf8_lossy(name).into_owned(), value));
            Ok(())
        }
    }

    fn out_src() -> OutLinkSrc<'static> {
        OutLinkSrc {
            putf: false,
            notify: OutNotify::None,
            field: "OUT",
        }
    }

    /// A plain OUT link whose target record is NOT local must write
    /// through the external put path — C `dbPutLink` routes a non-local
    /// (CA) link's write to `dbCaPutLink`, never a local `dbPut`. The
    /// pre-fix code called `put_pv_already_locked` on a name that does
    /// not exist locally; the write was silently dropped. The recording
    /// lset must now capture the value.
    #[epics_macros_rs::epics_test]
    async fn nonlocal_db_out_link_writes_through_external_put() {
        let db = PvDatabase::new();
        // C `dbCaLinkInit`: no build starts the `dbCaLink` worker here.
        db.ca_link_init();
        let puts = Arc::new(Mutex::new(Vec::new()));
        db.register_link_set("ca", Arc::new(RecordingLset { puts: puts.clone() }))
            .await;
        db.process_cp_links_on_connect().await;

        // "OTHER:PV" is never added as a local record -> non-local.
        let link = DbLink::new(
            "OTHER:PV",
            LinkProcessPolicy::NoProcess,
            MonitorSwitch::NoMaximize,
        );
        let mut visited = crate::server::database::ProcStack::new();
        db.write_db_link_value(&link, EpicsValue::Double(42.0), out_src(), &mut visited);
        // Staged on the link-put queue and returned, as C `dbCaPutLink`
        // does (`dbCa.c:593-595`); `dbCaSync` (`dbCa.c:1126-1129`) is the
        // barrier that makes the wire write observable.
        db.sync_external_link_puts().await;

        let captured = puts.lock().unwrap();
        assert_eq!(
            captured.len(),
            1,
            "non-local OUT-link write must reach the external put path exactly once"
        );
        assert_eq!(captured[0].0, "OTHER:PV");
        assert!(matches!(captured[0].1, EpicsValue::Double(v) if v == 42.0));
    }

    /// A local OUT-link write must NOT divert to the external put path —
    /// the locality dispatch only reroutes non-local targets. The local
    /// record receives the value; the lset records nothing.
    #[epics_macros_rs::epics_test]
    async fn local_db_out_link_writes_local_not_external() {
        let db = PvDatabase::new();
        let puts = Arc::new(Mutex::new(Vec::new()));
        db.register_link_set("ca", Arc::new(RecordingLset { puts: puts.clone() }))
            .await;
        db.add_record("TGT", Box::new(CalcRecord::new("0")))
            .await
            .unwrap();

        let link = DbLink::new(
            "TGT",
            LinkProcessPolicy::NoProcess,
            MonitorSwitch::NoMaximize,
        );
        let mut visited = crate::server::database::ProcStack::new();
        db.write_db_link_value(&link, EpicsValue::Double(7.0), out_src(), &mut visited);

        assert!(
            puts.lock().unwrap().is_empty(),
            "a local OUT-link write must not reach the external put path"
        );
        assert!(
            matches!(db.get_pv("TGT.VAL").unwrap(), EpicsValue::Double(v) if v == 7.0),
            "the local target must hold the written value"
        );
    }

    /// A link set whose forward handles record every `scan_forward` so a test
    /// can assert a non-DB FLNK reached the external forward path
    /// (C `dbScanFwdLink` → `lset->scanForward`) instead of being dropped
    /// by the DB-only `flnk_name` filter. `connected=false` models a
    /// disconnected link (pvxs `pvaScanForward`'s `!valid()` gate).
    struct ForwardingLset {
        forwards: Arc<Mutex<Vec<String>>>,
        connected: bool,
    }
    #[async_trait::async_trait]
    impl LinkSet for ForwardingLset {
        fn is_connected(&self, _: LinkRef<'_>) -> bool {
            self.connected
        }
        fn get_cached_value(&self, _: LinkRef<'_>) -> Option<EpicsValue> {
            None
        }
        async fn get_value(&self, link: LinkRef<'_>) -> Option<EpicsValue> {
            self.get_cached_value(link)
        }
        fn forward_handle(
            &self,
            link: LinkRef<'_>,
        ) -> Box<dyn crate::server::database::LinkForward> {
            let name = link.name;
            Box::new(RecordedForward {
                name: String::from_utf8_lossy(name).into_owned(),
                forwards: self.forwards.clone(),
                connected: self.connected,
            })
        }
    }

    struct RecordedForward {
        name: String,
        forwards: Arc<Mutex<Vec<String>>>,
        connected: bool,
    }

    impl crate::server::database::LinkForward for RecordedForward {
        fn scan_forward(&self) -> Result<(), crate::server::database::LinkForwardRefused> {
            self.forwards.lock().unwrap().push(self.name.clone());
            if self.connected {
                Ok(())
            } else {
                Err(crate::server::database::LinkForwardRefused)
            }
        }
    }

    /// `scan_forward_external_pv` must run the handle the registered lset
    /// opened the link with — the FWD-link twin of
    /// `write_external_pv` for OUT writes (C `dbScanFwdLink` →
    /// `lset->scanForward`).
    #[epics_macros_rs::epics_test]
    async fn external_forward_link_dispatches_through_scan_forward() {
        let db = PvDatabase::new();
        let forwards = Arc::new(Mutex::new(Vec::new()));
        db.register_link_set(
            "pva",
            Arc::new(ForwardingLset {
                forwards: forwards.clone(),
                connected: true,
            }),
        )
        .await;

        let link = crate::server::record::ParsedLink::Pva("OTHER:PROC".into());
        db.bind_external_forward(&link);
        let flnk = crate::server::database::LinkId {
            rec: None,
            field: "FLNK",
            install: crate::server::database::LinkInstall::Detached,
        };
        db.scan_forward_external_pv(flnk, link.external_forward().unwrap())
            .expect("a connected forward must succeed");

        let captured = forwards.lock().unwrap();
        assert_eq!(captured.len(), 1);
        assert_eq!(captured[0], "OTHER:PROC");
    }

    /// End-to-end: a record whose FLNK targets an external `pva://` PV
    /// must fire its forward handle when it processes — the
    /// non-DB FLNK dispatch the DB-only `flnk_name` filter previously
    /// dropped. C `recGblFwdLink` runs `dbScanFwdLink` for every FLNK
    /// regardless of link kind.
    #[epics_macros_rs::epics_test]
    async fn record_processing_fires_external_forward_link() {
        let db = PvDatabase::new();
        let forwards = Arc::new(Mutex::new(Vec::new()));
        db.register_link_set(
            "pva",
            Arc::new(ForwardingLset {
                forwards: forwards.clone(),
                connected: true,
            }),
        )
        .await;
        db.add_record("SRC", Box::new(CalcRecord::new("0")))
            .await
            .unwrap();
        if let Some(rec) = db.get_record("SRC") {
            let mut inst = rec.write();
            inst.put_common_field("FLNK", EpicsValue::String("pva://TARGET".into()))
                .unwrap();
        }
        // The lset opens the link at iocInit, as C's does.
        db.ioc_init().await;

        let mut visited = crate::server::database::ProcStack::new();
        db.process_record_with_links("SRC", &mut visited)
            .await
            .unwrap();

        let captured = forwards.lock().unwrap();
        assert_eq!(
            captured.len(),
            1,
            "an external pva:// FLNK must fire scan_forward exactly once"
        );
        assert_eq!(captured[0], "TARGET");
    }

    /// A disconnected non-retry external FLNK is NOT dropped silently: the
    /// lset returns Err and the owning record takes a *pending*
    /// LINK/INVALID alarm — pvxs `pvaScanForward`'s
    /// `recGblSetSevrMsg(LINK_ALARM, INVALID_ALARM, "Disconn")`, which
    /// writes `nsta`/`nsev`/`namsg` (promoted by the next
    /// `recGblResetAlarms`, matching the C late-set inside `recGblFwdLink`).
    #[epics_macros_rs::epics_test]
    async fn disconnected_external_forward_link_raises_pending_link_invalid() {
        let db = PvDatabase::new();
        let forwards = Arc::new(Mutex::new(Vec::new()));
        db.register_link_set(
            "pva",
            Arc::new(ForwardingLset {
                forwards: forwards.clone(),
                connected: false,
            }),
        )
        .await;
        db.add_record("SRC", Box::new(CalcRecord::new("0")))
            .await
            .unwrap();
        if let Some(rec) = db.get_record("SRC") {
            let mut inst = rec.write();
            inst.put_common_field("FLNK", EpicsValue::String("pva://TARGET".into()))
                .unwrap();
        }
        // The lset opens the link at iocInit, as C's does.
        db.ioc_init().await;

        let mut visited = crate::server::database::ProcStack::new();
        db.process_record_with_links("SRC", &mut visited)
            .await
            .unwrap();

        assert_eq!(
            forwards.lock().unwrap().len(),
            1,
            "scan_forward is still attempted on a disconnected link"
        );
        let rec = db.get_record("SRC").unwrap();
        let inst = rec.read();
        assert_eq!(inst.common.nsev, AlarmSeverity::Invalid);
        assert_eq!(
            inst.common.nsta,
            crate::server::recgbl::alarm_status::LINK_ALARM
        );
        assert_eq!(inst.common.namsg, "Disconn");
    }

    /// C `dbAddLink` opens a link put at runtime (`dbLink.c:146-191`): a
    /// `pva` FLNK written after the lset registered and after iocInit is
    /// bound by the put itself and dispatches on the next process.
    #[epics_macros_rs::epics_test]
    async fn external_forward_link_added_at_runtime_dispatches() {
        let db = PvDatabase::new();
        let forwards = Arc::new(Mutex::new(Vec::new()));
        db.register_link_set(
            "pva",
            Arc::new(ForwardingLset {
                forwards: forwards.clone(),
                connected: true,
            }),
        )
        .await;
        db.add_record("SRC", Box::new(CalcRecord::new("0")))
            .await
            .unwrap();
        db.ioc_init().await;
        db.put_record_field_from_ca("SRC", "FLNK", EpicsValue::String("pva://TARGET".into()))
            .await
            .unwrap();

        let mut visited = crate::server::database::ProcStack::new();
        db.process_record_with_links("SRC", &mut visited)
            .await
            .unwrap();

        assert_eq!(*forwards.lock().unwrap(), ["TARGET"]);
        assert_eq!(
            db.get_record("SRC").unwrap().read().common.nsev,
            AlarmSeverity::NoAlarm
        );
    }

    /// A forward whose slot no lset bound does nothing: with no `pva` lset
    /// registered nothing opens `pva://TARGET`, and C `dbScanFwdLink` skips a
    /// link with no `lset` (`dbLink.c:475-481`) without an alarm.
    #[epics_macros_rs::epics_test]
    async fn unbound_external_forward_link_is_a_silent_no_op() {
        let db = PvDatabase::new();
        db.add_record("SRC", Box::new(CalcRecord::new("0")))
            .await
            .unwrap();
        db.get_record("SRC")
            .unwrap()
            .write()
            .put_common_field("FLNK", EpicsValue::String("pva://TARGET".into()))
            .unwrap();
        db.ioc_init().await;

        let mut visited = crate::server::database::ProcStack::new();
        db.process_record_with_links("SRC", &mut visited)
            .await
            .unwrap();

        let rec = db.get_record("SRC").unwrap();
        let inst = rec.read();
        assert_eq!(
            (inst.common.nsev, inst.common.nsta),
            (AlarmSeverity::NoAlarm, 0)
        );
        assert_eq!(inst.common.namsg, "");
        assert_eq!(
            (inst.common.sevr, inst.common.stat),
            (AlarmSeverity::NoAlarm, 0)
        );
    }
}

#[cfg(test)]
mod cp_link_locality_tests {
    use crate::server::database::PvDatabase;
    use crate::server::record::ParsedLink;
    use crate::server::records::ai::AiRecord;

    /// C `dbInitLink` (`dbLink.c:118-122`) tests `CA`/`CP`/`CPP` before
    /// locality and skips `dbDbInitLink` when one is set, so a CP link is a
    /// CA link whether its record is here (`SRC`) or not (`OTHER:PV`), and
    /// each is registered as the trigger of its holder.
    #[epics_macros_rs::epics_test]
    async fn a_cp_link_is_a_ca_link_whatever_its_target() {
        let db = PvDatabase::new();
        for name in ["SRC", "LOCAL", "REMOTE"] {
            db.add_record(name, Box::new(AiRecord::new(0.0)))
                .await
                .unwrap();
        }
        for (holder, inp) in [("LOCAL", "SRC CP"), ("REMOTE", "OTHER:PV CPP")] {
            let rec = db.get_record(holder).unwrap();
            let mut inst = rec.write();
            inst.common.text.inp = inp.into();
            inst.install_link_cache(
                "INP",
                std::sync::Arc::new(crate::server::record::parse_link_v2(inp)),
            )
            .expect("INP is a common link");
        }

        db.initialize_link_locality().await;
        db.setup_cp_links().await;

        for (holder, pv) in [("LOCAL", "SRC"), ("REMOTE", "OTHER:PV")] {
            let inp = db.get_record(holder).unwrap().read().parsed_inp.clone();
            match inp {
                ParsedLink::Ca(ca) => assert_eq!(ca.pv, pv),
                other => panic!("{holder}: a CP link is a CA link, got {other:?}"),
            }
            let targets = db.get_external_cp_targets(pv.as_bytes());
            assert_eq!(targets.len(), 1, "{holder}");
            assert_eq!(targets[0].record, holder);
        }
        assert!(!db.get_external_cp_targets(b"SRC")[0].passive_only);
        assert!(db.get_external_cp_targets(b"OTHER:PV")[0].passive_only);
    }
}

#[cfg(test)]
mod external_link_pv_name_tests {
    use crate::server::database::PvDatabase;
    use crate::server::records::ai::AiRecord;
    use crate::server::records::ao::AoRecord;

    /// The declared external-PV set is per-PV and direction-agnostic, which
    /// is what makes it the right thing to count a link registry against:
    /// a link set holds one entry per PV, so two records reading one
    /// upstream PV are one entry, and an OUT link counts exactly like an IN
    /// link. iocInit's own console counts are per link FIELD and therefore
    /// disagree by construction — `realtime-ca-ioc`'s banner used to compare a
    /// registry count against nothing at all and print `0`.
    #[epics_macros_rs::epics_test]
    async fn declared_external_pvs_are_deduped_direction_agnostic_and_sorted() {
        let db = PvDatabase::new();
        for (name, rec) in [
            ("IN1", Box::new(AiRecord::new(0.0)) as Box<_>),
            ("IN2", Box::new(AiRecord::new(0.0)) as Box<_>),
            ("LOCAL_SRC", Box::new(AiRecord::new(1.0)) as Box<_>),
            ("LOCAL_HOLDER", Box::new(AiRecord::new(0.0)) as Box<_>),
        ] {
            db.add_record(name, rec).await.unwrap();
        }
        db.add_record("OUT1", Box::new(AoRecord::new(0.0)))
            .await
            .unwrap();

        // Two records, one upstream PV, two spellings: the bare ` CA`
        // modifier and the explicit scheme. One registry entry.
        db.get_record("IN1").unwrap().write().common.text.inp = "UP:ZED CP".into();
        db.get_record("IN2").unwrap().write().common.text.inp = "ca://UP:ZED CP".into();
        // An OUT link reaches `dbCaAddLink` in C exactly as an IN link does.
        db.get_record("OUT1").unwrap().write().common.text.out = "ca://UP:ALPHA".into();
        // A link to a record this IOC does have is local and must not count
        // (a CP one would: it is a CA link, `dbLink.c:118-122`).
        db.get_record("LOCAL_HOLDER")
            .unwrap()
            .write()
            .common
            .text
            .inp = "LOCAL_SRC PP".into();

        db.initialize_link_locality().await;

        assert_eq!(
            db.external_link_pv_names().await,
            vec!["UP:ALPHA".to_string(), "UP:ZED".to_string()],
            "the declared set must be deduped across spellings, hold OUT links, \
             exclude local targets, and come back sorted"
        );
    }

    /// A database with no external link declares the empty set — the case
    /// the banner reports as `0/0`, which is a healthy boot, not a failure
    /// to connect.
    #[epics_macros_rs::epics_test]
    async fn a_database_with_no_external_link_declares_none() {
        let db = PvDatabase::new();
        db.add_record("PLAIN", Box::new(AiRecord::new(0.0)))
            .await
            .unwrap();
        db.initialize_link_locality().await;
        assert!(db.external_link_pv_names().await.is_empty());
    }
}

#[cfg(test)]
mod nonlocal_db_link_read_tests {
    use crate::server::database::{LinkRef, LinkSet, PvDatabase};
    use crate::server::record::{ParsedLink, parse_link_v2};
    use crate::types::EpicsValue;
    use std::sync::Arc;

    /// A `ca` network half whose monitor cache holds what `serve` answers.
    struct NetworkStub(fn(&[u8]) -> Option<EpicsValue>);
    #[async_trait::async_trait]
    impl LinkSet for NetworkStub {
        fn is_connected(&self, link: LinkRef<'_>) -> bool {
            let name = link.name;
            (self.0)(name).is_some()
        }
        fn get_cached_value(&self, link: LinkRef<'_>) -> Option<EpicsValue> {
            let name = link.name;
            (self.0)(name)
        }
        async fn get_value(&self, link: LinkRef<'_>) -> Option<EpicsValue> {
            let name = link.name;
            (self.0)(name)
        }
    }

    pub(super) async fn serve_network(db: &PvDatabase, serve: fn(&[u8]) -> Option<EpicsValue>) {
        db.register_link_set("ca", Arc::new(NetworkStub(serve)))
            .await;
        db.process_cp_links_on_connect().await;
    }

    /// C `dbInitLink` (`dbLink.c:118-128`, the call being `dbCaAddLink` at
    /// `:128`) makes a PV link whose target is
    /// not a local record a CA link — even with NO `CP`/`CPP`/`CA`
    /// modifier (`dbDbInitLink` fails to resolve it locally and falls
    /// through to `dbCaAddLink`). So a plain `INP="OTHER:PV"`
    /// to a non-local target must read the remote value, not return
    /// `None`. Pre-fix the `Db` read arm read only the local DB (`get_pv`)
    /// and dropped the non-local target.
    #[epics_macros_rs::epics_test]
    async fn plain_nonlocal_db_link_reads_via_the_ca_link_set() {
        let db = PvDatabase::new();
        // Stand-in for the CA network: resolve OTHER:PV remotely.
        serve_network(&db, |name| {
            (name == b"OTHER:PV").then_some(EpicsValue::Double(42.0))
        })
        .await;

        let link = parse_link_v2("OTHER:PV");
        assert!(
            matches!(link, ParsedLink::Db(_)),
            "a bare non-scheme name parses to a Db link, got {link:?}"
        );

        let mut visited = crate::server::database::ProcStack::new();
        let v = db
            .read_link_value_and_alarm_at_source(None, &link, false, &mut visited)
            .0;
        assert_eq!(
            v,
            Some(EpicsValue::Double(42.0)),
            "a plain non-local Db link must resolve through the ca link set (C dbCaAddLink fallback)"
        );
    }

    /// A multi-input-style link re-parsed from its raw string each cycle
    /// (calc `INPA`..`INPL`, `DOL`) routes its value read through
    /// `read_link_with_alarm`; the same locality rule must hold there, so
    /// a non-local target reads the remote value and surfaces no
    /// local-record alarm.
    #[epics_macros_rs::epics_test]
    async fn nonlocal_db_link_value_and_alarm_via_external() {
        let db = PvDatabase::new();
        serve_network(&db, |name| {
            (name == b"OTHER:PV").then_some(EpicsValue::Double(5.0))
        })
        .await;

        let link = parse_link_v2("OTHER:PV");
        let (fetch, alarm, _) = db.read_link_with_alarm(None, &link, false);
        assert_eq!(
            fetch.value(),
            Some(EpicsValue::Double(5.0)),
            "non-local Db link value must come from the ca link set"
        );
        // The CA network half reports no severity, so the bare
        // external alarm path reports None — never a fabricated local
        // record alarm for a record that does not exist in this IOC.
        assert!(
            alarm.is_none(),
            "non-local Db link must not fabricate a local-record alarm, got {alarm:?}"
        );
    }

    /// Owner path: a Db link whose target IS a local record still reads
    /// the local database and never consults the ca link set.
    #[epics_macros_rs::epics_test]
    async fn local_db_link_reads_local_not_external() {
        let db = PvDatabase::new();
        db.add_pv("SRC", EpicsValue::Double(7.0)).await.unwrap();
        // A resolver returning a DIFFERENT value proves the local read
        // wins and the external path is not taken for a local target.
        serve_network(&db, |_| Some(EpicsValue::Double(-1.0))).await;

        let link = parse_link_v2("SRC");
        let mut visited = crate::server::database::ProcStack::new();
        let v = db
            .read_link_value_and_alarm_at_source(None, &link, false, &mut visited)
            .0;
        assert_eq!(
            v,
            Some(EpicsValue::Double(7.0)),
            "a local Db link must read the local DB, not the ca link set"
        );
    }
}

/// C's link-side metadata fetch — `dbGetGraphicLimits` & siblings
/// (`dbLink.c:344-393`) — one case per boundary the C code distinguishes,
/// not one per narrative.
#[cfg(test)]
mod link_metadata_tests {
    use crate::server::database::PvDatabase;
    use crate::server::record::parse_link_v2;
    use crate::server::records::ai::AiRecord;
    use crate::server::records::stringout::StringoutRecord;
    use crate::server::records::waveform::WaveformRecord;
    use crate::types::DbFieldType;

    /// BOUNDARY: constant link.
    ///
    /// `dbConst_lset` leaves all five metadata slots NULL
    /// (`dbConstLink.c:234-248`), so `dbGetGraphicLimits`'s
    /// `!plset->getGraphicLimits` test returns `S_db_noLSET` and writes
    /// NOTHING (`dbLink.c:358-359`).
    ///
    /// This is the case that decides the oracle's `field(INPA,"5")` records:
    /// C's answer is neither a propagated limit nor a DBF-type-range default.
    /// The caller keeps its pre-filled buffer.
    #[epics_macros_rs::epics_test]
    async fn constant_link_reports_no_metadata() {
        let db = PvDatabase::new();
        let mut visited = std::collections::HashSet::new();
        let link = parse_link_v2("5");
        assert!(
            matches!(link, crate::server::record::ParsedLink::Constant(_)),
            "fixture must actually be a constant link, got {link:?}"
        );
        assert_eq!(
            db.link_metadata(&link, &mut visited),
            None,
            "a constant link has no metadata lset slots: C returns S_db_noLSET"
        );
    }

    /// BOUNDARY: db link -> field whose record type HAS the slots.
    ///
    /// `ai` supplies every numeric slot, and `aiRecord.c::get_graphic_double`
    /// serves VAL from `prec->hopr`/`prec->lopr`. `dbDbGetGraphicLimits`
    /// returns those verbatim (`dbDbLink.c:280-295`).
    #[epics_macros_rs::epics_test]
    async fn db_link_to_supported_field_propagates_target_limits() {
        let db = PvDatabase::new();
        let mut src = AiRecord::new(1.0);
        src.hopr = 10.0;
        src.lopr = -10.0;
        src.egu = "mm".into();
        src.prec = 3;
        db.add_record("SRC", Box::new(src)).await.unwrap();

        let mut visited = std::collections::HashSet::new();
        let meta = db
            .link_metadata(&parse_link_v2("SRC"), &mut visited)
            .expect("a local db link to an existing field reports metadata");

        assert_eq!(meta.graphic_limits, Some((-10.0, 10.0)));
        assert_eq!(meta.units.as_deref(), Some("mm"));
        assert_eq!(meta.precision, Some(3));
    }

    /// BOUNDARY: db link -> field whose record type has NO slots at all.
    ///
    /// `dbGet` does not fail here — it fills a default, turns the option bit
    /// off, and returns 0, so `dbDbGet*` reads the default out and reports it.
    /// `stringout` `#define`s every `get_*` NULL (`stringoutRecord.c:64-83`).
    ///
    /// The defaults are NOT uniform, which is the whole point of this case:
    /// `get_graphics`/`get_control` `memset` to zero (`dbAccess.c:241-242`,
    /// `:281-283`) while `get_alarm` seeds `{epicsNAN×4}` and assigns
    /// unconditionally (`dbAccess.c:294,317-329`).
    #[epics_macros_rs::epics_test]
    async fn db_link_to_unsupported_field_yields_c_defaults_not_none() {
        let db = PvDatabase::new();
        db.add_record("STR", Box::new(StringoutRecord::new("hello")))
            .await
            .unwrap();

        let mut visited = std::collections::HashSet::new();
        let meta = db
            .link_metadata(&parse_link_v2("STR"), &mut visited)
            .expect("dbGet returns 0 even when every option is turned off");

        assert_eq!(
            meta.graphic_limits,
            Some((0.0, 0.0)),
            "no get_graphic_double: C memsets the buffer to zero and returns 0"
        );
        assert_eq!(
            meta.control_limits,
            Some((0.0, 0.0)),
            "no get_control_double: C memsets the buffer to zero and returns 0"
        );
        let (lolo, low, high, hihi) = meta.alarm_limits.expect("alarm limits are written");
        assert!(
            lolo.is_nan() && low.is_nan() && high.is_nan() && hihi.is_nan(),
            "no get_alarm_double: C's pre-filled epicsNAN survives, NOT zero \
             (dbAccess.c:290) — got ({lolo}, {low}, {high}, {hihi})"
        );
        assert_eq!(meta.precision, Some(0), "no get_precision: C memsets to 0");
        assert_eq!(
            meta.units.as_deref(),
            Some(""),
            "no get_units: C memsets the units buffer"
        );
    }

    /// BOUNDARY: db link -> record type that has SOME slots but not
    /// `get_alarm_double` (`waveformRecord.c` `#define get_alarm_double NULL`).
    ///
    /// Pins that the no-support default is decided per slot, not per record:
    /// the graphic limits still come from the target's own support while the
    /// alarm limits fall back to NaN.
    #[epics_macros_rs::epics_test]
    async fn db_link_alarm_default_is_per_slot_not_per_record() {
        let db = PvDatabase::new();
        db.add_record("WF", Box::new(WaveformRecord::new(8, DbFieldType::Double)))
            .await
            .unwrap();

        let mut visited = std::collections::HashSet::new();
        let meta = db
            .link_metadata(&parse_link_v2("WF"), &mut visited)
            .expect("waveform reports metadata");

        let (lolo, low, high, hihi) = meta.alarm_limits.expect("alarm limits are written");
        assert!(
            lolo.is_nan() && low.is_nan() && high.is_nan() && hihi.is_nan(),
            "waveform NULLs get_alarm_double: NaN, not zero"
        );
        assert!(
            meta.graphic_limits.is_some(),
            "waveform keeps get_graphic_double, so that slot is still served"
        );
    }

    /// BOUNDARY: the link points back at a field already on this fetch chain.
    ///
    /// C's `DBLINK_FLAG_VISITED` guard: a re-entered link leaves `status` at
    /// its `S_dbLib_badLink` initialiser and writes nothing
    /// (`dbDbLink.c:239-261`) — without it, a record that sources its own
    /// metadata from an input link pointing back at itself recurses forever.
    #[epics_macros_rs::epics_test]
    async fn revisited_db_link_target_is_refused() {
        let db = PvDatabase::new();
        db.add_record("SRC", Box::new(AiRecord::new(1.0)))
            .await
            .unwrap();

        let link = parse_link_v2("SRC");
        let mut visited = std::collections::HashSet::new();
        assert!(
            db.link_metadata(&link, &mut visited).is_some(),
            "first visit resolves"
        );
        assert!(
            visited.is_empty(),
            "the guard must be CLEARED after the fetch (C clears the flag at \
             dbDbLink.c:257), or a diamond onto one target would fail"
        );

        // Simulate being re-entered from inside SRC's own metadata fetch. The
        // key is `DbLink::pvname` — the link text itself, so two links whose
        // text differs stay separate entries even when they address the same
        // field.
        visited.insert("SRC".into());
        assert_eq!(
            db.link_metadata(&link, &mut visited),
            None,
            "a link already on the chain must write nothing"
        );
    }

    /// A db link naming a field the target record does not have has no
    /// `dbAddr` in C — `dbNameToAddr` fails at link-init time, so no lset is
    /// installed and `dbGetGraphicLimits` returns `S_db_noLSET`.
    #[epics_macros_rs::epics_test]
    async fn db_link_to_missing_field_reports_no_metadata() {
        let db = PvDatabase::new();
        db.add_record("SRC", Box::new(AiRecord::new(1.0)))
            .await
            .unwrap();
        let mut visited = std::collections::HashSet::new();
        assert_eq!(
            db.link_metadata(&parse_link_v2("SRC.NOSUCHFIELD"), &mut visited),
            None,
        );
    }
}

/// The reactor precondition on the external-link gate.
///
/// `tokio_backend` only: on the exec backend `BlockingBridge` is a ZST and
/// `try_capture` always succeeds, so a database with no reactor is a state
/// that cannot be constructed there.
#[cfg(all(test, tokio_backend))]
mod no_reactor_gate_tests {
    use std::sync::Arc;

    use crate::server::database::PvDatabase;
    use crate::server::database::link_set::{LinkPutOp, LinkRef, LinkSet, PutAdmission};
    use crate::types::EpicsValue;

    /// Connected, writable, and willing — so the only thing that can refuse
    /// the put is the missing reactor.
    struct WillingLset;

    #[async_trait::async_trait]
    impl LinkSet for WillingLset {
        fn is_connected(&self, _: LinkRef<'_>) -> bool {
            true
        }
        fn put_admission(&self, _: LinkRef<'_>) -> PutAdmission {
            PutAdmission::Connected
        }
        async fn get_value(&self, _: LinkRef<'_>) -> Option<EpicsValue> {
            None
        }
        async fn put_value(
            &self,
            _: LinkRef<'_>,
            _: EpicsValue,
            _: LinkPutOp,
        ) -> Result<(), String> {
            Ok(())
        }
    }

    /// BOUNDARY: database built with no tokio runtime entered anywhere.
    ///
    /// A plain `#[test]`, deliberately: the point is that no runtime exists
    /// on this thread when `PvDatabase::new` runs, which is the `reactor:
    /// None` state the queue's field documents. `block_on_sync` off a runtime
    /// selects `park_on`, and registration awaits only in-process locks.
    ///
    /// The lset says `Connected`, so before this gate the write was staged
    /// and its network half was dispatched onto the background executor —
    /// which deliberately has no `tokio::net` reactor, so a `ca://` target
    /// panicked there instead of failing here. C never queues work no owner
    /// can perform: where it cannot do the put it refuses at staging with
    /// `-1` (`db/dbCa.c:529-532` (`dbCaPutLinkCallback`); epics-base
    /// R7.0.10), and it cannot reach a no-owner state at all because
    /// `dbCaLinkInitImpl` creates the `dbCaLink` worker unconditionally and
    /// blocks until it is up (`db/dbCa.c:322` (`dbCaLinkInitImpl`), `:342`,
    /// `:344`; R7.0.10).
    #[test]
    fn an_external_put_is_refused_when_the_database_captured_no_reactor() {
        let db = crate::runtime::task::block_on_sync(async {
            let db = PvDatabase::new();
            db.register_link_set("ca", Arc::new(WillingLset)).await;
            db
        })
        .expect("a plain test thread is blockable");

        let err = db
            .external_put_admitted(crate::server::record::ExternalRef {
                name: b"ca://SOME:PV",
                open: &crate::server::record::OpenLatch::default(),
            })
            .expect_err("a database with no reactor must refuse the external put");
        assert!(
            err.to_string().contains("no tokio runtime"),
            "the refusal must name the missing reactor, got: {err}"
        );
    }
}

impl crate::server::record::record_instance::LinkTargetResolver for PvDatabase {
    fn name_revision(&self) -> u64 {
        // Monotonic counters, so their sum changes whenever any does.
        self.inner.records.revision() + self.inner.aliases.revision() + self.simple_pvs_revision()
    }

    fn local_target(
        &self,
        link: &crate::server::record::ParsedLink,
    ) -> Option<crate::server::record::record_instance::ResolvedTarget> {
        let crate::server::record::ParsedLink::Db(db) = link else {
            return None;
        };
        self.db_link_addr(db)
    }
}

#[cfg(test)]
mod db_link_addr_tests {
    use crate::server::database::{ProcStack, PvDatabase};
    use crate::server::record::{ParsedLink, parse_link_v2};
    use crate::server::records::ai::AiRecord;
    use crate::types::EpicsValue;
    use std::sync::Arc;

    fn read(db: &PvDatabase, link: &ParsedLink) -> Option<EpicsValue> {
        db.read_link_value_and_alarm_at_source(None, link, false, &mut ProcStack::new())
            .0
    }

    /// A simple PV spelling the target (`DST.HOPR`) is settled in the link's
    /// answer like an address: once the first cycle has resolved it, a cycle
    /// takes no `simple_pvs` lock (held here across it) and still reads the
    /// PV, through the reader's `INP` and through a link read alike.
    #[epics_macros_rs::epics_test]
    async fn a_shadowed_target_is_settled_once_per_revision() {
        let db = PvDatabase::new();
        db.add_record("DST", Box::new(AiRecord::new(1.0)))
            .await
            .unwrap();
        db.add_record("RD", Box::new(AiRecord::new(0.0)))
            .await
            .unwrap();
        db.get_record("RD")
            .unwrap()
            .write()
            .put_common_field("INP", EpicsValue::String("DST.HOPR".into()))
            .unwrap();
        db.add_pv("DST.HOPR", EpicsValue::Double(5.0))
            .await
            .unwrap();
        let link = parse_link_v2("DST.HOPR");
        db.process_record("RD").await.unwrap();
        assert_eq!(read(&db, &link), Some(EpicsValue::Double(5.0)));

        let held = db.inner.simple_pvs.lock();
        let (tx, rx) = std::sync::mpsc::channel();
        let cycle = db.clone();
        std::thread::spawn(move || {
            let r = cycle.process_record_with_links_sync(b"RD", &mut ProcStack::new());
            let v = read(&cycle, &link);
            let _ = tx.send((r.is_ok(), v));
        });
        let done = rx.recv_timeout(std::time::Duration::from_secs(5));
        drop(held);
        assert_eq!(
            done.expect("a settled cycle took the simple_pvs lock"),
            (true, Some(EpicsValue::Double(5.0)))
        );
        assert_eq!(db.get_pv("RD").unwrap(), EpicsValue::Double(5.0));
    }

    /// Unchanged name maps: the second read goes through the address the
    /// first one resolved, not a second lookup.
    #[epics_macros_rs::epics_test]
    async fn an_unchanged_name_map_reuses_the_resolved_address() {
        let db = PvDatabase::new();
        db.add_record("SRC", Box::new(AiRecord::new(1.0)))
            .await
            .unwrap();
        let ParsedLink::Db(link) = parse_link_v2("SRC") else {
            panic!("a record name parses to a Db link");
        };
        let first = db.db_link_addr(&link).expect("SRC is local");
        let second = db.db_link_addr(&link).expect("SRC is local");
        assert!(Arc::ptr_eq(&first.rec, &second.rec));
    }

    /// A record added after the answer settled `External` is found by the
    /// next read, which re-resolves at the moved revision and settles the
    /// answer: the read after it needs no resolve.
    #[epics_macros_rs::epics_test]
    async fn a_target_added_after_an_external_answer_is_re_resolved() {
        use crate::server::record::DbLinkAnswer;
        use crate::server::record::record_instance::LinkTargetResolver;
        let db = PvDatabase::new();
        let parsed = parse_link_v2("NEW");
        let ParsedLink::Db(link) = &parsed else {
            panic!("a record name parses to a Db link");
        };
        assert!(matches!(db.db_link_answer(link), DbLinkAnswer::External));
        db.add_record("NEW", Box::new(AiRecord::new(7.0)))
            .await
            .unwrap();
        assert_eq!(read(&db, &parsed), Some(EpicsValue::Double(7.0)));
        let settled = link.answer(db.name_revision(), || panic!("the answer was not settled"));
        assert!(matches!(settled, DbLinkAnswer::Local(_)));
    }

    /// A record removed and re-added under the link's name moves the
    /// revision: the link reads the new record, not the one it resolved.
    #[epics_macros_rs::epics_test]
    async fn a_re_added_target_is_re_resolved() {
        let db = PvDatabase::new();
        db.add_record("SRC", Box::new(AiRecord::new(1.0)))
            .await
            .unwrap();
        let link = parse_link_v2("SRC");
        assert_eq!(read(&db, &link), Some(EpicsValue::Double(1.0)));
        // Held, so the cached `Weak` still upgrades: only the revision can
        // tell the link its address is stale.
        let _old = db.get_record("SRC").unwrap();
        assert!(db.remove_record("SRC").await);
        db.add_record("SRC", Box::new(AiRecord::new(7.0)))
            .await
            .unwrap();
        assert_eq!(read(&db, &link), Some(EpicsValue::Double(7.0)));
    }

    /// The read carries the source's alarm only for a link that inherits
    /// it: NMS (the default) carries none, MS carries it.
    #[epics_macros_rs::epics_test]
    async fn only_an_inheriting_link_carries_the_source_alarm() {
        let db = PvDatabase::new();
        db.add_record("SRC", Box::new(AiRecord::new(1.0)))
            .await
            .unwrap();
        db.add_record("READER", Box::new(AiRecord::new(0.0)))
            .await
            .unwrap();
        let reader = db.get_record("READER").unwrap();
        let alarm_of = |text: &str| {
            let link = parse_link_v2(text);
            let (value, alarm, _) =
                db.read_link_value_and_alarm_at_source(None, &link, false, &mut ProcStack::new());
            assert_eq!(value, Some(EpicsValue::Double(1.0)));
            (
                alarm.is_some(),
                db.with_frame(&reader, |f| db.input_link_inheritance(f, &link, alarm))
                    .is_some(),
            )
        };
        assert_eq!(alarm_of("SRC"), (false, false));
        assert_eq!(alarm_of("SRC NMS"), (false, false));
        assert_eq!(alarm_of("SRC MS"), (true, true));
    }

    /// A target that leaves the database turns the link non-local: the
    /// cached address is not read, the ca link set is.
    #[epics_macros_rs::epics_test]
    async fn a_removed_target_falls_through_to_the_external_path() {
        let db = PvDatabase::new();
        super::nonlocal_db_link_read_tests::serve_network(&db, |name| {
            (name == b"SRC").then_some(EpicsValue::Double(9.0))
        })
        .await;
        db.add_record("SRC", Box::new(AiRecord::new(1.0)))
            .await
            .unwrap();
        let link = parse_link_v2("SRC");
        assert_eq!(read(&db, &link), Some(EpicsValue::Double(1.0)));
        // Held, so the cached `Weak` still upgrades: only the revision can
        // tell the link its address is stale.
        let _old = db.get_record("SRC").unwrap();
        assert!(db.remove_record("SRC").await);
        assert_eq!(read(&db, &link), Some(EpicsValue::Double(9.0)));
    }

    /// The shadow test is decided by the link's spelling: a `VAL` target
    /// cannot be shadowed, a simple PV spelled `REC.FLD` shadows that field.
    #[epics_macros_rs::epics_test]
    async fn only_a_non_val_target_can_be_shadowed() {
        let db = PvDatabase::new();
        db.add_record("SRC", Box::new(AiRecord::new(1.0)))
            .await
            .unwrap();
        db.add_pv("SRC.DESC", EpicsValue::Double(5.0))
            .await
            .unwrap();
        for (text, is_val) in [
            ("SRC", true),
            ("SRC.VAL", true),
            ("SRC.{\"arr\":{}}", true),
            ("SRC.DESC", false),
        ] {
            let ParsedLink::Db(link) = parse_link_v2(text) else {
                panic!("{text} parses to a Db link");
            };
            assert_eq!(link.target_is_val(), is_val, "{text}");
        }
        assert_eq!(
            read(&db, &parse_link_v2("SRC")),
            Some(EpicsValue::Double(1.0))
        );
        assert_eq!(
            read(&db, &parse_link_v2("SRC.DESC")),
            Some(EpicsValue::Double(5.0))
        );
    }

    async fn add_soft_ai(db: &PvDatabase, name: &str, inp: &str) {
        db.add_record(name, Box::new(AiRecord::new(0.0)))
            .await
            .unwrap();
        db.get_record(name)
            .unwrap()
            .write()
            .put_common_field_db_load("INP", EpicsValue::String(inp.into()))
            .unwrap();
    }

    async fn ai_val(db: &PvDatabase, name: &str) -> Option<EpicsValue> {
        db.get_record(name).unwrap().read().resolve_field("VAL")
    }

    /// The held-guard INP read lends a strong address; removing the target
    /// releases it (the owner sweep), and a target re-added under the name
    /// is re-resolved and read on the next cycle.
    #[epics_macros_rs::epics_test]
    async fn a_removed_inp_target_releases_the_lent_address() {
        let db = PvDatabase::new();
        db.add_record("SRC", Box::new(AiRecord::new(1.0)))
            .await
            .unwrap();
        add_soft_ai(&db, "READER", "SRC").await;
        db.process_record("READER").await.unwrap();
        assert_eq!(ai_val(&db, "READER").await, Some(EpicsValue::Double(1.0)));
        let src = db.get_record("SRC").unwrap();
        assert_eq!(
            Arc::strong_count(&src),
            4,
            "the map, the record table, the lent address, this test"
        );
        assert!(db.remove_record("SRC").await);
        assert_eq!(
            Arc::strong_count(&src),
            2,
            "the lent address outlived the removal"
        );
        drop(src);
        db.add_record("SRC", Box::new(AiRecord::new(7.0)))
            .await
            .unwrap();
        db.process_record("READER").await.unwrap();
        assert_eq!(ai_val(&db, "READER").await, Some(EpicsValue::Double(7.0)));
    }

    /// A destroyed reader drops its lent address: the target keeps only
    /// the handles that are not the reader's.
    #[epics_macros_rs::epics_test]
    async fn a_removed_reader_releases_its_lent_address() {
        let db = PvDatabase::new();
        db.add_record("SRC", Box::new(AiRecord::new(1.0)))
            .await
            .unwrap();
        add_soft_ai(&db, "READER", "SRC").await;
        db.process_record("READER").await.unwrap();
        let src = db.get_record("SRC").unwrap();
        assert_eq!(Arc::strong_count(&src), 4);
        let reader = db.get_record("READER").unwrap();
        assert!(db.remove_record("READER").await);
        assert_eq!(
            Arc::strong_count(&src),
            3,
            "the map, the record table and this test"
        );
        drop(reader);
    }

    /// Two records whose INPs read each other hold each other through
    /// their lent addresses; the database's drop still frees both.
    #[epics_macros_rs::epics_test]
    async fn dropping_the_database_frees_inps_that_read_each_other() {
        let db = PvDatabase::new();
        add_soft_ai(&db, "CYC:A", "CYC:B").await;
        add_soft_ai(&db, "CYC:B", "CYC:A").await;
        db.process_record("CYC:A").await.unwrap();
        db.process_record("CYC:B").await.unwrap();
        let a = db.get_record("CYC:A").unwrap();
        let b = db.get_record("CYC:B").unwrap();
        assert_eq!((Arc::strong_count(&a), Arc::strong_count(&b)), (4, 4));
        let (weak_a, weak_b) = (Arc::downgrade(&a), Arc::downgrade(&b));
        drop((a, b, db));
        assert!(weak_a.upgrade().is_none() && weak_b.upgrade().is_none());
    }

    /// The `DBR_DOUBLE` read answers what the general path's `DBR_DOUBLE`
    /// read does, alarm included, on each side of each boundary of its slot
    /// path: a `VAL` slot with and without the alarm, a filtered `VAL`, a
    /// non-`VAL` slot, a `VAL` with no slot, an array, a string that does
    /// and does not convert, and a field that does not exist.
    #[epics_macros_rs::epics_test]
    async fn the_double_read_answers_what_the_general_read_does() {
        use crate::server::record::{LinkReadAs, Record};
        use crate::server::records::{
            calc::CalcRecord, stringin::StringinRecord, waveform::WaveformRecord,
        };
        let db = PvDatabase::new();
        db.add_record("SRC", Box::new(AiRecord::new(1.5)))
            .await
            .unwrap();
        let mut calc = CalcRecord::new("A");
        calc.put_field("A", EpicsValue::Double(2.5)).unwrap();
        calc.put_field("VAL", EpicsValue::Double(4.5)).unwrap();
        db.add_record("C", Box::new(calc)).await.unwrap();
        let mut wf = WaveformRecord::new(4, crate::types::DbFieldType::Double);
        wf.put_field("VAL", EpicsValue::DoubleArray(vec![6.0, 7.0].into()))
            .unwrap();
        db.add_record("W", Box::new(wf)).await.unwrap();
        db.add_record("S", Box::new(StringinRecord::new("3.25")))
            .await
            .unwrap();
        db.add_record("T", Box::new(StringinRecord::new("abc")))
            .await
            .unwrap();
        for (text, slot_path, expect) in [
            ("C MS", true, Some(4.5)),
            ("C NMS", true, Some(4.5)),
            ("C.VAL MS", true, Some(4.5)),
            ("C.{\"arr\":{}} MS", false, Some(4.5)),
            ("C.A MS", false, Some(2.5)),
            ("SRC MS", false, Some(1.5)),
            ("W MS", false, Some(6.0)),
            ("S MS", false, Some(3.25)),
            ("T MS", false, None),
        ] {
            let parsed = parse_link_v2(text);
            let ParsedLink::Db(link) = &parsed else {
                panic!("{text} parses to a Db link");
            };
            let at = db.db_link_addr(link).expect("a local target");
            assert_eq!(
                link.target_is_val()
                    && link.target().json_suffix.is_none()
                    && crate::server::database::field_io::double_slot(at.put.field()).is_some(),
                slot_path,
                "{text}"
            );
            let held = db.lock_record(&link.target().record);
            let (value, alarm) = db.read_db_link_f64_at(held.set(), link, &at);
            let (fetch, general_alarm) = db.read_link_value_as_with_alarm(
                None,
                &parsed,
                LinkReadAs::Double,
                &mut ProcStack::new(),
            );
            let general = match fetch {
                crate::server::recgbl::simm::LinkFetch::Value(v) => Some(v),
                _ => None,
            };
            assert_eq!(value, expect, "{text}");
            assert_eq!(value.map(EpicsValue::Double), general, "{text}");
            assert_eq!(
                alarm.as_ref().map(|a| (format!("{:?}", a.alarm), a.record)),
                general_alarm
                    .as_ref()
                    .map(|a| (format!("{:?}", a.alarm), a.record)),
                "{text}"
            );
        }
        // `dbFindFieldPart` names neither, so the link has no address.
        for text in ["C.NOPE MS", "C.val MS"] {
            let ParsedLink::Db(link) = parse_link_v2(text) else {
                panic!("{text} parses to a Db link");
            };
            assert!(db.db_link_addr(&link).is_none(), "{text}");
        }
        let ParsedLink::Db(link) = parse_link_v2("C.CALC$ MS") else {
            panic!("C.CALC$ parses to a Db link");
        };
        assert!(
            db.db_link_addr(&link).is_some(),
            "the `$` is no part of the field"
        );
    }

    /// `dbGetLink(DBR_CHAR)` of a non-char source converts it (softIoc
    /// R7.0.10.1 printf `%ls`): one element of a numeric scalar, a string
    /// parsed, and text that does not parse fails the read.
    #[epics_macros_rs::epics_test]
    async fn a_char_read_of_a_non_char_source_is_its_dbr_char_conversion() {
        use crate::server::recgbl::simm::LinkFetch;
        use crate::server::record::LinkReadAs;
        use crate::server::records::{longin::LonginRecord, stringin::StringinRecord};
        let db = PvDatabase::new();
        for (name, v) in [("L7", 7), ("L300", 300), ("LN", -200)] {
            db.add_record(name, Box::new(LonginRecord::new(v)))
                .await
                .unwrap();
        }
        for (name, v) in [("S42", "42"), ("SABC", "abc")] {
            db.add_record(name, Box::new(StringinRecord::new(v)))
                .await
                .unwrap();
        }
        let read_as = LinkReadAs::CharArrayAsString { max_elements: 40 };
        for (src, want) in [
            ("L7", Some(&[7u8][..])),
            ("L300", Some(&[44])),
            ("LN", Some(&[56])),
            ("S42", Some(&[42])),
            ("SABC", None),
        ] {
            let (fetch, _) = db.read_link_value_as_with_alarm(
                None,
                &parse_link_v2(src),
                read_as,
                &mut ProcStack::new(),
            );
            let got = match fetch {
                LinkFetch::Value(EpicsValue::String(s)) => Some(s.as_bytes().to_vec()),
                _ => None,
            };
            assert_eq!(got.as_deref(), want, "{src}");
        }
    }

    /// The read with the target in hand answers what the general path
    /// does, alarm included, at every request type: a failed conversion
    /// (an array as a string, `abc` as a double) inherits nothing.
    #[epics_macros_rs::epics_test]
    async fn the_held_target_read_answers_what_the_general_read_does() {
        use crate::server::record::{LinkReadAs, Record};
        use crate::server::records::{
            calc::CalcRecord, stringin::StringinRecord, waveform::WaveformRecord,
        };
        let db = PvDatabase::new();
        db.add_record("SRC", Box::new(AiRecord::new(1.5)))
            .await
            .unwrap();
        let mut calc = CalcRecord::new("A");
        calc.put_field("VAL", EpicsValue::Double(4.5)).unwrap();
        db.add_record("C", Box::new(calc)).await.unwrap();
        let mut wf = WaveformRecord::new(4, crate::types::DbFieldType::Char);
        wf.put_field("VAL", EpicsValue::CharArray(b"hi".to_vec().into()))
            .unwrap();
        db.add_record("W", Box::new(wf)).await.unwrap();
        db.add_record("T", Box::new(StringinRecord::new("abc")))
            .await
            .unwrap();
        let requests = [
            LinkReadAs::Native,
            LinkReadAs::Double,
            LinkReadAs::Long,
            LinkReadAs::ULong,
            LinkReadAs::UShort,
            LinkReadAs::Int64,
            LinkReadAs::Enum,
            LinkReadAs::String,
            LinkReadAs::CharArrayAsString { max_elements: 40 },
        ];
        for text in ["C MS", "C NMS", "SRC MS", "W MS", "T MS"] {
            let parsed = parse_link_v2(text);
            let ParsedLink::Db(link) = &parsed else {
                panic!("{text} parses to a Db link");
            };
            let at = db.db_link_addr(link).expect("a local target");
            for read_as in requests {
                let held = db.lock_record(&link.target().record);
                let (value, alarm) = db.read_db_link_as_at(held.set(), &parsed, &at, read_as);
                let (fetch, general_alarm) =
                    db.read_link_value_as_with_alarm(None, &parsed, read_as, &mut ProcStack::new());
                let general = match fetch {
                    crate::server::recgbl::simm::LinkFetch::Value(v) => Some(v),
                    _ => None,
                };
                assert_eq!(value, general, "{text} as {read_as:?}");
                assert_eq!(
                    alarm.as_ref().map(|a| (format!("{:?}", a.alarm), a.record)),
                    general_alarm
                        .as_ref()
                        .map(|a| (format!("{:?}", a.alarm), a.record)),
                    "{text} as {read_as:?}"
                );
            }
        }
    }

    /// Each integer request on the edge C's GET table defines: an integer
    /// source narrows by cast, a string parses with base auto-detect, and a
    /// `DBR_ENUM` read of a string fails. A failed read inherits no alarm.
    #[epics_macros_rs::epics_test]
    async fn the_integer_requests_convert_as_c_dbget_does() {
        use crate::server::record::LinkReadAs;
        use crate::server::records::{
            int64in::Int64inRecord, longin::LonginRecord, mbbi::MbbiRecord,
            stringin::StringinRecord,
        };
        let db = PvDatabase::new();
        db.add_record("BIG", Box::new(Int64inRecord::new(0x1_0000_0005)))
            .await
            .unwrap();
        db.add_record("NEG", Box::new(LonginRecord::new(-1)))
            .await
            .unwrap();
        db.add_record("HEX", Box::new(StringinRecord::new("0x10")))
            .await
            .unwrap();
        db.add_record("EXP", Box::new(StringinRecord::new("1e3")))
            .await
            .unwrap();
        db.add_record("WORD", Box::new(StringinRecord::new("One")))
            .await
            .unwrap();
        db.add_record("M", Box::new(MbbiRecord::new(2)))
            .await
            .unwrap();
        for (text, read_as, expect) in [
            ("BIG", LinkReadAs::Long, Some(EpicsValue::Long(5))),
            (
                "BIG",
                LinkReadAs::Int64,
                Some(EpicsValue::Int64(0x1_0000_0005)),
            ),
            (
                "BIG",
                LinkReadAs::Double,
                Some(EpicsValue::Double(4294967301.0)),
            ),
            ("NEG", LinkReadAs::UShort, Some(EpicsValue::UShort(65535))),
            ("NEG", LinkReadAs::ULong, Some(EpicsValue::ULong(u32::MAX))),
            ("NEG", LinkReadAs::Enum, Some(EpicsValue::Enum(65535))),
            ("HEX", LinkReadAs::Long, Some(EpicsValue::Long(16))),
            ("HEX", LinkReadAs::Int64, Some(EpicsValue::Int64(16))),
            ("HEX", LinkReadAs::UShort, Some(EpicsValue::UShort(16))),
            ("EXP", LinkReadAs::ULong, Some(EpicsValue::ULong(1000))),
            ("WORD", LinkReadAs::Long, None),
            ("WORD", LinkReadAs::Enum, None),
            ("HEX", LinkReadAs::Enum, None),
            ("M", LinkReadAs::Enum, Some(EpicsValue::Enum(2))),
        ] {
            let parsed = parse_link_v2(format!("{text} MS"));
            let ParsedLink::Db(link) = &parsed else {
                panic!("{text} parses to a Db link");
            };
            let at = db.db_link_addr(link).expect("a local target");
            let held = db.lock_record(&link.target().record);
            let (value, alarm) = db.read_db_link_as_at(held.set(), &parsed, &at, read_as);
            assert_eq!(value, expect, "{text} as {read_as:?}");
            assert_eq!(alarm.is_some(), expect.is_some(), "{text} as {read_as:?}");
        }
    }

    /// A link reads through the cached slot exactly what the by-name read
    /// answers, on each side of the slot boundary: a slot-addressed DOUBLE,
    /// a string field, an undeclared name and a numeric field with no slot.
    #[epics_macros_rs::epics_test]
    async fn the_slot_read_answers_what_the_name_read_does() {
        use crate::server::record::Record;
        use crate::server::record::record_instance::FieldAddr;
        use crate::server::records::calc::CalcRecord;
        let db = PvDatabase::new();
        let mut calc = CalcRecord::new("A+B");
        calc.put_field("A", EpicsValue::Double(2.5)).unwrap();
        calc.put_field("VAL", EpicsValue::Double(f64::NAN)).unwrap();
        db.add_record("C", Box::new(calc)).await.unwrap();
        db.add_record("SRC", Box::new(AiRecord::new(4.0)))
            .await
            .unwrap();
        let cases = [
            ("C.A", "C", "A", true),
            ("C.VAL", "C", "VAL", true),
            ("C.CALC", "C", "CALC", false),
            ("C.NOPE", "C", "NOPE", false),
            ("C.RTYP", "C", "RTYP", false),
            ("SRC.VAL", "SRC", "VAL", false),
        ];
        for (text, rec, field, slotted) in cases {
            let cell = db.get_record(rec).unwrap();
            assert_eq!(
                FieldAddr::resolve_in(&cell, field).slot().is_some(),
                slotted,
                "{text}"
            );
            let by_name = cell.read().resolve_field_upper(field);
            let got = read(&db, &parse_link_v2(text));
            match (&got, &by_name) {
                (Some(EpicsValue::Double(a)), Some(EpicsValue::Double(b))) => {
                    assert_eq!(a.to_bits(), b.to_bits(), "{text}")
                }
                _ => assert_eq!(got, by_name, "{text}"),
            }
        }
        assert_eq!(
            read(&db, &parse_link_v2("C.A")),
            Some(EpicsValue::Double(2.5))
        );
    }
}

#[cfg(test)]
mod record_link_uses_tests {
    use super::*;

    fn instance(rtype: &str) -> RecordInstance {
        let record = crate::server::db_loader::create_record(rtype).unwrap();
        RecordInstance::new_boxed("R", record)
    }

    #[test]
    fn every_record_link_use_names_a_slotted_link() {
        for (rtype, _) in RECORD_LINK_USES {
            check_record_links(&instance(rtype)).unwrap();
        }
    }

    #[test]
    fn a_type_reading_a_non_link_through_record_link_fails_to_load() {
        let err = check_record_links_in(&instance("fanout"), &[("fanout", &[&["SELM"]])])
            .unwrap_err()
            .to_string();
        assert!(err.contains("fanout.SELM"), "{err}");
    }
}

/// One CP/CPP link's trigger: the channel it watches and CPP's gate.
struct CpEdge {
    /// The link's PV text.
    pv: RecordName,
    passive_only: bool,
    /// The link's identity: its channel's events process the holder.
    open: crate::server::record::OpenLatch,
}

impl CpEdge {
    fn external(&self) -> crate::server::record::ExternalRef<'_> {
        crate::server::record::ExternalRef {
            name: &self.pv,
            open: &self.open,
        }
    }
}

/// The external registry's key: the PV the calink/pvalink monitor dispatches
/// under, which the lset has stripped of its scheme.
fn external_cp_key(pv: &[u8]) -> &[u8] {
    pv.strip_prefix(b"ca://")
        .or_else(|| pv.strip_prefix(b"pva://"))
        .unwrap_or(pv)
}
/// Why [`PvDatabase::write_external_pv`] staged nothing.
#[derive(Debug)]
pub(crate) enum ExternalPutRefusal {
    /// The link is down or read-only: C `dbCaPutLinkCallback`'s silent `-1`
    /// (`db/dbCa.c:558-560`). The record's LINK alarm is the whole report.
    NotConnected,
    /// No link set or reactor can carry the write, a state C cannot reach.
    Unserviceable(String),
    /// The value does not convert to the channel's native type: the
    /// conversion status `dbCaPutLinkCallback` returns, with nothing logged.
    Unconvertible(String),
}

impl ExternalPutRefusal {
    /// What the refusal owes the errlog: nothing for a down link, as in C.
    pub(crate) fn report(&self, name: &[u8]) {
        if let Self::Unserviceable(why) = self {
            crate::runtime::log::errlog_vprintf(format_args!(
                "OUT-link write to external PV '{}' failed: {why}\n",
                String::from_utf8_lossy(name)
            ));
        }
    }
}

impl std::fmt::Display for ExternalPutRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotConnected => f.write_str(
                "not connected, or connected without write access \
                 (db/dbCa.c:558-560 (dbCaPutLinkCallback))",
            ),
            Self::Unserviceable(why) | Self::Unconvertible(why) => f.write_str(why),
        }
    }
}

/// Spec §5 test 17: an OUT target outside the source's held set. Every DB
/// link merges its two sets at iocInit, so the state is reached as
/// breakpoint.rs's fixtures reach theirs: the link poked in after the sets
/// were built. rs only; C has no observable equivalent.
#[cfg(test)]
mod out_link_held_set_tests {
    use crate::server::database::{ProcStack, PvDatabase};
    use crate::server::record::{LinkFieldType, parse_link_field};
    use crate::server::records::ao::AoRecord;
    use crate::types::EpicsValue;

    fn point_out(db: &PvDatabase, from: &str, to: &str) {
        let rec = db.get_record(from).expect("record");
        let mut inst = rec.write();
        inst.common.text.out = to.into();
        inst.set_link_cache("OUT", parse_link_field(to, LinkFieldType::Out));
    }

    fn num(db: &PvDatabase, pv: &str) -> f64 {
        db.get_pv(pv).unwrap().to_f64().unwrap()
    }

    // RTEMS-EXEC-MODEL-ALLOW(1): the runtime only awaits add_record and the frame; passes on the exec backend.
    #[tokio::test]
    async fn a_target_outside_the_held_set_is_written_like_one_inside() {
        let db = PvDatabase::new();
        for name in ["HS:IN", "HS:SI", "HS:OUT", "HS:SO"] {
            db.add_record(name, Box::new(AoRecord::new(0.0)))
                .await
                .expect("add_record");
        }
        point_out(&db, "HS:SI", "HS:IN.VAL PP");
        db.build_lock_sets();
        point_out(&db, "HS:SO", "HS:OUT.VAL PP");
        let members = |r: &str| db.lock_set_of(r).expect("a set").members;
        assert!(members("HS:SI").iter().any(|n| n == "HS:IN"));
        assert!(
            !members("HS:SO").iter().any(|n| n == "HS:OUT"),
            "the precondition: HS:OUT is outside HS:SO's set"
        );

        for (src, tgt) in [("HS:SI", "HS:IN"), ("HS:SO", "HS:OUT")] {
            db.get_record(src)
                .unwrap()
                .write()
                .record
                .set_val(EpicsValue::Double(4.0))
                .unwrap();
            db.process_record_with_links(src, &mut ProcStack::new())
                .await
                .expect("process");
            assert_eq!(num(&db, tgt), 4.0, "{tgt}");
            assert_eq!(num(&db, &format!("{tgt}.UDF")), 0.0, "{tgt} processed");
            assert_eq!(num(&db, &format!("{tgt}.SEVR")), 0.0, "{tgt}");
            assert_eq!(num(&db, &format!("{src}.SEVR")), 0.0, "{src}");
        }
    }
}

#[cfg(test)]
mod enum_read_held_set_tests {
    use crate::server::database::{FrameGate, ProcStack, PvDatabase};
    use crate::server::recgbl::EventMask;
    use crate::server::recgbl::simm::LinkFetch;
    use crate::server::record::{LinkReadAs, Record, parse_link_v2};
    use crate::server::records::{ao::AoRecord, bi::BiRecord};
    use crate::types::{DbFieldType, EpicsValue};

    /// `cvt_st_e` answers the state's index and leaves the source alone (C's
    /// `put_enum_str` stores its VAL, a defect not copied), from a reader
    /// whose set holds the source (the borrow) and from one whose set does
    /// not (the acquire).
    // RTEMS-EXEC-MODEL-ALLOW(1): the runtime only awaits add_record; passes on the exec backend.
    #[tokio::test]
    async fn a_state_name_read_answers_the_index_in_or_out_of_the_held_set() {
        let db = PvDatabase::new();
        let mut bsrc = BiRecord::new(0);
        bsrc.put_field("ZNAM", EpicsValue::String("Low".into()))
            .unwrap();
        bsrc.put_field("ONAM", EpicsValue::String("High".into()))
            .unwrap();
        db.add_record("BSRC", Box::new(bsrc)).await.unwrap();
        db.add_record("OTHER", Box::new(AoRecord::new(0.0)))
            .await
            .unwrap();
        db.build_lock_sets();
        assert!(
            !db.lock_set_of("OTHER")
                .unwrap()
                .members
                .iter()
                .any(|n| n == "BSRC")
        );
        let mut posts = db
            .get_record("BSRC")
            .unwrap()
            .write()
            .add_subscriber(
                "VAL",
                1,
                DbFieldType::Enum,
                (EventMask::VALUE | EventMask::LOG).bits(),
            )
            .unwrap();
        while posts.try_recv().is_ok() {}

        for (reader, field, want) in [("BSRC", "ONAM", 1), ("OTHER", "ZNAM", 0)] {
            let cell = db.get_record(reader).unwrap();
            let gate = FrameGate::Taken(db.lock_instance(&cell));
            let (fetch, _) = db.read_link_value_as_with_alarm(
                Some(gate.frame(&cell)),
                &parse_link_v2(format!("BSRC.{field} NPP")),
                LinkReadAs::Enum,
                &mut ProcStack::new(),
            );
            drop(gate);
            assert!(
                matches!(&fetch, LinkFetch::Value(EpicsValue::Enum(i)) if *i == want),
                "{reader}: {fetch:?}"
            );
            assert_eq!(db.get_pv("BSRC").unwrap(), EpicsValue::Enum(0), "{reader}");
            assert!(posts.try_recv().is_err(), "{reader}: nothing posted");
        }
    }
}

/// An external forward link is dispatched from the record's own parse, as C
/// hands `plink` to `scanForward`: while the lset runs, nothing but the
/// record (and this test) holds the parse — no per-cycle `Arc` clone, whose
/// count pair C does not pay.
#[cfg(test)]
mod external_forward_borrow_tests {
    use crate::server::database::{LinkRef, LinkSet, ProcStack, PvDatabase};
    use crate::server::ioc_builder::IocBuilder;
    use crate::server::record::ParsedLink;
    use crate::types::EpicsValue;
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};

    /// A `pva` lset whose forward handle reads the count of `watched` as it
    /// forwards.
    #[derive(Default)]
    struct CountAtForward(Arc<Counts>);

    #[derive(Default)]
    struct Counts {
        watched: Mutex<Option<Arc<ParsedLink>>>,
        seen: Mutex<Vec<usize>>,
    }

    #[async_trait::async_trait]
    impl LinkSet for CountAtForward {
        fn is_connected(&self, _: LinkRef<'_>) -> bool {
            true
        }
        async fn get_value(&self, _: LinkRef<'_>) -> Option<EpicsValue> {
            None
        }
        fn forward_handle(&self, _: LinkRef<'_>) -> Box<dyn crate::server::database::LinkForward> {
            Box::new(CountingForward(self.0.clone()))
        }
    }

    struct CountingForward(Arc<Counts>);

    impl crate::server::database::LinkForward for CountingForward {
        fn scan_forward(&self) -> Result<(), crate::server::database::LinkForwardRefused> {
            if let Some(link) = &*self.0.watched.lock().unwrap() {
                self.0.seen.lock().unwrap().push(Arc::strong_count(link));
            }
            Ok(())
        }
    }

    async fn counts_at_forward(db: &PvDatabase, lset: &CountAtForward, rec: &str, field: &str) {
        let link = db
            .get_record(rec)
            .unwrap()
            .read()
            .stored_link_shared(field)
            .cloned()
            .unwrap();
        assert!(matches!(*link, ParsedLink::Pva(_)), "{rec}.{field}");
        *lset.0.watched.lock().unwrap() = Some(link);
        lset.0.seen.lock().unwrap().clear();
        db.process_record_with_links(rec, &mut ProcStack::new())
            .await
            .unwrap();
        *lset.0.watched.lock().unwrap() = None;
        // The record's and this test's.
        assert_eq!(*lset.0.seen.lock().unwrap(), [2], "{rec}.{field}");
    }

    #[epics_macros_rs::epics_test]
    async fn an_external_flnk_and_lnkn_are_dispatched_unshared() {
        let db = IocBuilder::new()
            .register_link_type("pva", |_db| async { Vec::new() })
            .db_string(
                r#"
record(calc, "S") { field(CALC, "VAL+1") field(FLNK, "pva://T:A") }
record(fanout, "F") { field(LNK1, "pva://T:B") }
"#,
                &HashMap::new(),
            )
            .unwrap()
            .build()
            .await
            .unwrap()
            .0;
        let lset = Arc::new(CountAtForward::default());
        db.register_link_set("pva", lset.clone()).await;
        db.ioc_init().await;
        counts_at_forward(&db, &lset, "S", "FLNK").await;
        counts_at_forward(&db, &lset, "F", "LNK1").await;
    }

    /// What the target's subroutine sees: the count of the watched source
    /// link and of the target's cell, as it processes.
    type Watch = (
        Option<(
            Arc<crate::server::record::DbLink>,
            Arc<crate::server::record::RecordCell>,
        )>,
        Vec<(usize, usize)>,
    );
    static WATCH: Mutex<Watch> = Mutex::new((None, Vec::new()));

    #[epics_macros_rs::epics_test]
    async fn a_db_flnk_and_lnkn_are_dispatched_unshared() {
        let db = IocBuilder::new()
            .register_subroutine("watch", |_| {
                let mut w = WATCH.lock().unwrap();
                let counts =
                    w.0.as_ref()
                        .map(|(link, cell)| (Arc::strong_count(link), Arc::strong_count(cell)));
                w.1.extend(counts);
                Ok(0)
            })
            .db_string(
                r#"
record(sub, "T") { field(SNAM, "watch") }
record(calc, "S") { field(CALC, "VAL+1") field(FLNK, "T") }
record(fanout, "F") { field(LNK1, "T") }
"#,
                &HashMap::new(),
            )
            .unwrap()
            .build()
            .await
            .unwrap()
            .0;
        db.ioc_init().await;
        let cell = db.get_record("T").unwrap();
        for (rec, field) in [("S", "FLNK"), ("F", "LNK1")] {
            let link = db
                .get_record(rec)
                .unwrap()
                .read()
                .stored_link_shared(field)
                .cloned();
            let Some(ParsedLink::Db(link)) = link.as_deref() else {
                panic!("{rec}.{field} is not a DB link");
            };
            *WATCH.lock().unwrap() = (Some((link.clone(), cell.clone())), Vec::new());
            let before = (Arc::strong_count(link), Arc::strong_count(&cell));
            db.process_record_with_links(rec, &mut ProcStack::new())
                .await
                .unwrap();
            let seen = std::mem::take(&mut *WATCH.lock().unwrap()).1;
            // The forward shares neither the link nor the target.
            assert_eq!(seen, [before], "{rec}.{field}");
        }
    }

    /// After `iocInit` a forward is bound or CA, never a DB link without a
    /// target: one naming no local record is made CA there, a record added
    /// later binds its own forwards, and an existing CA forward stays CA.
    #[epics_macros_rs::epics_test]
    async fn a_forward_is_bound_or_ca_after_ioc_init() {
        use crate::server::records::{calc::CalcRecord, fanout::FanoutRecord};
        let db = IocBuilder::new()
            .db_string(
                r#"record(calc, "S") { field(FLNK, "LATE") }"#,
                &HashMap::new(),
            )
            .unwrap()
            .build()
            .await
            .unwrap()
            .0;
        db.ioc_init().await;
        let flnk = |db: &PvDatabase| {
            db.get_record("S")
                .unwrap()
                .read()
                .stored_link_shared("FLNK")
                .cloned()
                .unwrap()
        };
        assert!(matches!(*flnk(&db), ParsedLink::Ca(_)), "{:?}", flnk(&db));

        db.add_record("LATE", Box::new(CalcRecord::new("VAL+1")))
            .await
            .unwrap();
        let mut fan = FanoutRecord::new();
        fan.lnk1 = "LATE".into();
        db.add_record("U", Box::new(fan)).await.unwrap();
        assert!(
            matches!(*flnk(&db), ParsedLink::Ca(_)),
            "S.FLNK was rebound"
        );
        db.process_record_with_links("U", &mut ProcStack::new())
            .await
            .unwrap();
        assert_eq!(db.get_pv("LATE").unwrap().to_f64(), Some(1.0));
    }
}
