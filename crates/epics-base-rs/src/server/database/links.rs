use std::collections::HashSet;
use std::sync::Arc;

use crate::server::record::{AlarmSeverity, NotifyWaitSet, OutTarget, RecordInstance, ScanType};
use crate::types::{DbFieldType, EpicsValue, PvString};

use super::filters::ChannelName;
use super::link_set::LinkDbfType;
use super::{LinkPutOp, PvDatabase, SelmKind, SelmResult, dbr_ushort_cast, select_link_indices_ex};

/// The `record[.FIELD]` name a link target is addressed by in the local
/// database — the name with the channel filter already removed.
///
/// [`DbLink::pvname`](crate::server::record::DbLink::pvname) is C's verbatim
/// `pv_link.pvname`, so for `src.[2]` it is the whole string;
/// [`DbLink::target`](crate::server::record::DbLink::target) is the one place
/// that splits it, and this is the one place that puts the halves back
/// together for a local lookup. The external name stays `pvname`, which keeps
/// the filter because C hands it to `dbCaAddLink` unchanged.
fn local_pv_name(target: &ChannelName) -> String {
    if target.field == "VAL" {
        target.record.clone()
    } else {
        format!("{}.{}", target.record, target.field)
    }
}

/// **The one classifier of a process-time read that delivered no value.**
///
/// C `dbGetLink` reports `(status, buffer)`, and the two outcomes a `None`
/// collapses are not the same: a CONSTANT (or unset) link is `dbConstGetValue`
/// (`dbConstLink.c:219-225`) — status 0, `*pnRequest = 0`, buffer untouched —
/// while any other link that produced nothing FAILED and owes the reader a LINK
/// alarm. Every process-time reader ends here ([`PvDatabase::read_link_with_alarm`]
/// for the input-fetch/control-link paths, [`PvDatabase::read_link_value_as`] for
/// the `ReadDbLink` executor), so a constant cannot be no-data on one path and a
/// live value on another.
fn empty_read_fetch(
    link: &crate::server::record::ParsedLink,
) -> crate::server::recgbl::simm::LinkFetch {
    use crate::server::recgbl::simm::LinkFetch;
    if crate::server::recgbl::simm::is_constant(link) {
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
fn char_bytes_as_string(value: &EpicsValue, max_elements: usize) -> Option<PvString> {
    let bytes: &[u8] = match value {
        EpicsValue::CharArray(b) | EpicsValue::UCharArray(b) => b,
        EpicsValue::Char(b) | EpicsValue::UChar(b) => std::slice::from_ref(b),
        // Not a char-class source after all (the metadata said it was): C would
        // have copied raw bytes of whatever it found. There is no faithful
        // rendering, so deliver nothing — the caller treats it as a failed read.
        _ => return None,
    };
    let n = bytes.len().min(max_elements);
    let text = &bytes[..n];
    let end = text.iter().position(|&b| b == 0).unwrap_or(n);
    Some(PvString::from_bytes(text[..end].to_vec()))
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

/// seq desired-output link fields, index-aligned with `MultiOut::Seq`
/// (`seqRecord.c:86` — DOL0..DOLF). The names C's `dbLinkFieldName` yields
/// for the `setLinkAlarm` AMSG of a failed group read.
pub(crate) const DOL_LINK_FIELDS: [&str; 16] = [
    "DOL0", "DOL1", "DOL2", "DOL3", "DOL4", "DOL5", "DOL6", "DOL7", "DOL8", "DOL9", "DOLA", "DOLB",
    "DOLC", "DOLD", "DOLE", "DOLF",
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
    pub amsg: String,
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
    /// EVERY OUT-link put site uses this: the record's own OUT, the generic
    /// multi-output pairs, the fanout/dfanout/seq dispatch, the `WriteDbLink`
    /// / `WriteDbLinkNotify` process actions, and sseq's `put_link_notify`
    /// (sseqRecord.c: `dbPutLink` in `processCallback`, `recGblResetAlarms` in
    /// `asyncFinish` — the puts still precede the commit).
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
/// (`write_db_link_value`, C `dbDbPutValue` →
/// `recGblInheritSevrMsg`, dbDbLink.c:382-383, where `dest` is the
/// OUT-link target). One implementation keeps the two sides from
/// diverging — an earlier INPUT-side variant wrongly treated MS like
/// MSS (propagating source stat + amsg through plain MS).
pub(crate) fn inherit_sevr_msg(
    dest: &mut crate::server::record::CommonFields,
    ms: crate::server::record::MonitorSwitch,
    src: &LinkAlarm,
) {
    use crate::server::recgbl::{alarm_status, rec_gbl_set_sevr, rec_gbl_set_sevr_msg};
    use crate::server::record::{AlarmSeverity, MonitorSwitch};
    match ms {
        MonitorSwitch::Maximize => {
            rec_gbl_set_sevr(dest, alarm_status::LINK_ALARM, src.sevr);
        }
        MonitorSwitch::MaximizeIfInvalid => {
            if src.sevr == AlarmSeverity::Invalid {
                rec_gbl_set_sevr(dest, alarm_status::LINK_ALARM, src.sevr);
            }
        }
        MonitorSwitch::MaximizeStatus => {
            rec_gbl_set_sevr_msg(dest, src.stat, src.sevr, src.amsg.clone());
        }
        MonitorSwitch::NoMaximize => {} // NMS: do not propagate
    }
}

/// The source record's per-cycle state propagated to a DB OUT-link
/// target, captured once from the source and threaded to every OUT-link
/// write so all targets in one cycle see the same snapshot:
///
/// * `putf` / `notify` — the PUTF bit and put-notify wait-set that C
///   `processTarget` carries to each target (dbDbLink.c:460-474).
/// * `alarm` — the PENDING alarm that C `recGblInheritSevrMsg` folds
///   into the dest per the link's MS-class switch (dbDbLink.c:382-383
///   reads `psrce->nsta`/`nsev`/`namsg`, before the source commits them).
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
    pub notify: Option<&'a Arc<NotifyWaitSet>>,
    pub alarm: &'a LinkAlarm,
    pub field: &'a str,
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
    /// Does a target with this SCAN reach `processTarget` through this gate?
    fn admits(self, target_scan: ScanType) -> bool {
        match self {
            Self::ScanPassive => target_scan == ScanType::Passive,
            Self::ProcField => true,
        }
    }
}

/// One `seq` link group — C `linkGrp { dly, dol, dov, lnk }`.
#[derive(Clone, Debug)]
pub(crate) struct SeqGroup {
    /// DOLn input link string (empty when unset).
    pub dol: String,
    /// LNKn output link string (empty when unset).
    pub lnk: String,
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
    /// fanout — 16 forward-link strings (LNK0..LNKF).
    Fanout(Vec<String>),
    /// dfanout — 16 output-link strings (OUTA..OUTP).
    Dfanout(Vec<String>),
    /// seq — 16 link groups (0..F).
    Seq(Vec<SeqGroup>),
}

impl MultiOut {
    /// Number of link slots — the `count` passed to the SELM selector.
    fn len(&self) -> usize {
        match self {
            MultiOut::Fanout(v) => v.len(),
            MultiOut::Dfanout(v) => v.len(),
            MultiOut::Seq(v) => v.len(),
        }
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

/// Which phase of a process cycle a multi-output record's links belong to.
///
/// The phase follows from what the links ARE, not from the record's name:
///
/// * A record whose links carry a VALUE (`dbPutLink`) — dfanout `OUTn`
///   (`dfanoutRecord.c:323`), seq `LNKn` (`seqRecord.c:264`) — dispatches
///   PRE-commit, because C issues those puts from inside `process()` /
///   `processCallback`, before `recGblResetAlarms` commits the cycle
///   (dfanout: `monitor()`, seq: `asyncFinish`, seqRecord.c:227). A failed
///   put's `LINK_ALARM`/`INVALID` and a `SELN`-out-of-range
///   `SOFT_ALARM`/`INVALID` must therefore land in the alarm THIS cycle
///   commits and posts.
/// * A record whose links only SCAN a target — fanout `LNK0..LNKF` are
///   `DBF_FWDLINK` (`fanoutRecord.dbd`), dispatched via `dbScanFwdLink`
///   (`fanoutRecord.c:110/121/138`) — dispatches in the post-commit
///   forward-link tail: no value, no put status, no alarm to fold.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum MultiOutPhaseKind {
    Output,
    ForwardLink,
}

/// The phase argument of [`PvDatabase::dispatch_multi_output`]. The `Output`
/// variant carries `skip_out` — the IVOA=Don't_drive veto as decided by the
/// cycle's single IVOA owner (`process_record_with_links_inner`), on the
/// severity `checkAlarms` produced. That is the one decision C makes
/// (`dfanoutRecord.c:128`: `if (prec->nsev < INVALID_ALARM) push_values();
/// else switch (ivoa)`), taken before ANY output runs; the IVOA=IVOV arm has
/// already stored IVOV in the record's own value field
/// ([`Record::apply_invalid_output_value`](crate::server::record::Record::apply_invalid_output_value) — C `prec->val = prec->ivov`,
/// dfanoutRecord.c:137), so the push here just reads VAL, as C's `push_values`
/// does.
#[derive(Clone, Copy)]
pub(crate) enum MultiOutPhase {
    Output { skip_out: bool },
    ForwardLink,
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

/// The phase a record type's multi-output links belong to — see
/// [`MultiOutPhaseKind`]. Both call sites (the pre-commit output stage and
/// the forward-link tail) run `dispatch_multi_output` unconditionally and
/// this single classifier decides which types act, so a record cannot be
/// dispatched in both phases or in neither.
pub(crate) fn multi_out_phase_of(record_type: &str) -> MultiOutPhaseKind {
    match record_type {
        "dfanout" | "seq" => MultiOutPhaseKind::Output,
        _ => MultiOutPhaseKind::ForwardLink,
    }
}

impl PvDatabase {
    /// Read a `Db`-variant link's value honoring C `dbInitLink`'s
    /// locality rule (`dbLink.c:118-128`, the call being `dbCaAddLink` at
    /// `:128` — see [`PvDatabase::setup_external_link_opens`]): a PV link
    /// whose target record exists in this IOC reads from the local
    /// database; a non-local target is a CA link — `dbDbInitLink` fails to
    /// resolve it locally and falls through to `dbCaAddLink`, so its value
    /// comes
    /// from the external resolver. Pre-fix every `Db` arm read only the
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
    fn read_db_link_value(&self, db: &crate::server::record::DbLink) -> Option<EpicsValue> {
        let target = db.target();
        let Some(suffix) = target.json_suffix.as_deref() else {
            return self.read_target_value(&target.record, &target.field);
        };
        // A filtered target that is NOT local stays whole: C hands
        // `pv_link.pvname` to `dbCaAddLink` filter and all
        // (`dbLink.c:128`), and the far end's `dbChannelCreate` builds the
        // chain. Applying it here as well would filter twice.
        if !self.has_name_no_resolve(&target.record) {
            return self.resolve_external_pv(&db.pvname());
        }
        // Local: C `dbDbGetValue` takes the "filter, array, or special"
        // arm (`dbDbLink.c:206-219`) — `db_create_read_log`, then
        // `dbChannelRunPreChain` and `dbChannelRunPostChain`, then
        // `dbChannelGet` with the resulting log. `src.[2]` therefore
        // delivers one element and `src.[2:4]` three, which is what makes
        // the scalar reader see 3 rather than the head of the waveform.
        // A chain that drops the log leaves `pfl` NULL and `dbChannelGet`
        // reads the raw field, so a drop falls back to the unfiltered
        // value here too — the same rule the CA read path applies
        // (`epics-ca-rs/src/server/tcp.rs:3466-3471`).
        let value = self.read_target_value(&target.record, &target.field)?;
        let chain = crate::server::database::filters::parse_filter_chain(suffix);
        Some(chain.apply_to_read_value(value.clone()).unwrap_or(value))
    }

    /// Read a `(record, field)` link target's value, dispatching by C
    /// `dbInitLink` locality (`dbLink.c:118-130`): a target present in
    /// this IOC reads from the local database (`get_pv`); a non-local
    /// target is a CA link, resolved through the external path
    /// (`dbCaGetLink`). The single owner of that locality decision for
    /// the value-read path — shared by [`Self::read_db_link_value`] (the
    /// record's own `Db` link) and the lnkCalc input loop
    /// ([`Self::evaluate_calc_link`]), whose `A..L` inputs are each their
    /// own `dbInitLink` link and so become CA links when non-local.
    fn read_target_value(&self, record: &str, field: &str) -> Option<EpicsValue> {
        let pv_name = if field == "VAL" {
            record.to_string()
        } else {
            format!("{record}.{field}")
        };
        if self.has_name_no_resolve(record) {
            self.get_pv(&pv_name).ok()
        } else {
            self.resolve_external_pv(&pv_name)
        }
    }

    /// Read a value from a parsed link (DB, Constant, or external Ca/Pva).
    ///
    /// `visited` / `depth` are the caller's processing-chain state so a
    /// PP source is processed within the same chain — see
    /// [`Self::process_passive_db_source`] for why a fresh set / depth 0
    /// would defeat the cycle guard.
    pub(crate) fn read_link_value(
        &self,
        link: &crate::server::record::ParsedLink,
        visited: &mut HashSet<String>,
        depth: usize,
    ) -> Option<EpicsValue> {
        match link {
            crate::server::record::ParsedLink::None => None,
            crate::server::record::ParsedLink::Ca(ca) => self.resolve_external_pv(&ca.pv),
            crate::server::record::ParsedLink::Pva(name) => self.resolve_external_pv(name),
            // Resolve through the per-link identity key (not bare `j.pv`)
            // so two same-PV structured links keep distinct configs —
            // matches the boundary key the write/scan/alarm paths use via
            // `external_pv_name` (pvxs per-link `pvaLinkConfig`,
            // ioc/pvalink.h:65).
            crate::server::record::ParsedLink::PvaJson(j) => {
                self.resolve_external_pv(&j.link_identity_key())
            }
            // A CONSTANT (or unset) link delivers NOTHING at process time —
            // `dbConstGetValue` (`dbConstLink.c:219-225`) sets `*pnRequest = 0`
            // and returns 0, so the reader's buffer keeps whatever it held. The
            // constant reaches the record exactly once, at init, through
            // [`super::PvDatabase::rec_gbl_init_constant_links`]. Handing the
            // parsed text back here is what let the `ReadDbLink` executor
            // re-apply a constant every cycle (sseq `SELL="3"` stomping a
            // client's `caput SELN 5`, a compress with `INP="5"` filling its
            // buffer with 5s). `None` here is classified as
            // [`LinkFetch::NoData`](crate::server::recgbl::simm::LinkFetch::NoData) — success, nothing delivered — by
            // [`empty_read_fetch`], never as a failed read.
            crate::server::record::ParsedLink::Constant(_) => None,
            crate::server::record::ParsedLink::Db(db) => {
                // PP: process source record if Passive before reading.
                // Threads the caller's `visited`/`depth` so an A↔B PP
                // cycle terminates at the existing cycle guard instead
                // of recursing with a fresh set.
                self.process_passive_db_source(db, visited, depth);
                self.read_db_link_value(db)
            }
            // Hardware links are dispatched by device support directly
            // — there's no canonical "value" available from a generic
            // read; return None so the framework treats the link as
            // unresolvable for value-read purposes.
            crate::server::record::ParsedLink::Hw(_) => None,
            // lnkCalc: fetch each input PV, evaluate the expr,
            // return the result. The timestamp half is not here — C keeps
            // it on the link (`clink->time`) and serves it from the lset
            // (`lnkCalc_getTimestampTag`), so this port answers it from
            // `db_get_time_stamp_tag`, the one owner of link→timestamp.
            crate::server::record::ParsedLink::Calc(calc) => self.evaluate_calc_link(calc),
            crate::server::record::ParsedLink::State(st) => Some(self.read_state_link(st)),
        }
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
    pub(crate) fn read_link_value_as(
        &self,
        link: &crate::server::record::ParsedLink,
        read_as: crate::server::record::LinkReadAs,
        visited: &mut HashSet<String>,
        depth: usize,
    ) -> crate::server::recgbl::simm::LinkFetch {
        use crate::server::recgbl::simm::LinkFetch;
        let Some(value) = self.read_link_value(link, visited, depth) else {
            return empty_read_fetch(link);
        };
        match self.apply_link_read_as(link, read_as, value) {
            Some(v) => LinkFetch::Value(v),
            None => LinkFetch::Failed,
        }
    }

    /// The `dbrType` conversion of one fetched link value — the single owner
    /// of C `dbGet`'s request-type switch for every framework input read
    /// (the pre-input [`Self::read_db_link_into_field`] stage, the single-INP
    /// soft fetch, the DOL fetch, the multi-input loop and the SIOL read).
    ///
    /// `None` is C's non-zero `dbGetLink` status (`dbConvert.c`'s NULL
    /// conversion slot), i.e. a FAILED read — never a silent no-op.
    pub(crate) fn apply_link_read_as(
        &self,
        link: &crate::server::record::ParsedLink,
        read_as: crate::server::record::LinkReadAs,
        value: EpicsValue,
    ) -> Option<EpicsValue> {
        use crate::server::record::LinkReadAs;
        match read_as {
            LinkReadAs::Native => Some(value),
            // C `dbGetLink(..., DBR_DOUBLE, ...)`: an array source contributes
            // its element 0 (C asks for one element, so `dbGet` converts offset
            // 0), the same rule the multi-input fetch applies.
            LinkReadAs::Double => {
                let scalar = if value.is_array() {
                    value.first_element()
                } else {
                    Some(value)
                };
                scalar
                    .and_then(|s| s.get_convert_f64())
                    .map(EpicsValue::Double)
            }
            LinkReadAs::String => self.dbr_string_of(link, &value).map(EpicsValue::String),
            LinkReadAs::CharArrayAsString { max_elements } => {
                char_bytes_as_string(&value, max_elements).map(EpicsValue::String)
            }
        }
    }

    /// The `DBR_STRING` form of a value just read from `link`: rendered by the
    /// SOURCE record when the link is local (its choice tables are what turn an
    /// `ENUM`/`MENU` index into a state label — C `getEnumString`), by the value
    /// itself otherwise (an external link / constant / lnkCalc result carries no
    /// reachable field metadata).
    fn dbr_string_of(
        &self,
        link: &crate::server::record::ParsedLink,
        value: &EpicsValue,
    ) -> Option<PvString> {
        if let crate::server::record::ParsedLink::Db(db) = link
            && let target = db.target()
            && self.has_name_no_resolve(&target.record)
            && let Some(rec) = self.get_record(&target.record)
        {
            let guard = rec.read();
            return guard.field_as_dbr_string(&target.field);
        }
        // An external ENUM value crosses the wire as a bare index; the label
        // table lives in the lset's cached metadata (CA: the one-shot
        // `DBR_CTRL_ENUM` attribute fetch — C `dbCa` instead keeps a second
        // `DBR_STRING` monitor for exactly this read, `dbCa.c` `pgetString`).
        // An index past the cached table falls through to the digits, the
        // same rule as [`value_as_dbr_string`]'s `EnumWithChoices` arm.
        if let EpicsValue::Enum(idx) = value
            && let Some(name) = link.external_pv_name()
            && let Some(m) = self.external_link_metadata(&name)
            && let Some(choices) = m.enum_choices
            && let Some(label) = choices.get(*idx as usize)
        {
            return Some(PvString::from(label.as_str()));
        }
        crate::server::record::value_as_dbr_string(value)
    }

    /// Read a link's current value WITHOUT processing a Passive source —
    /// the parity of C `dbGetLink` (`dbLink.c:325` → `dbTryGetLink` →
    /// `lset->getValue`), which fetches the value for *any* link type
    /// (DB / CA / PVA / constant / lnkCalc) and never processes the
    /// target record.
    ///
    /// Distinct from [`Self::read_link_value`], which threads
    /// `visited`/`depth` to PP-process a Passive DB source before reading
    /// (the INPUT-link path). `dbGetLink` does no such processing, so the
    /// DB arm here reads with a plain `get_pv` exactly as the pre-fix
    /// control-link sites did.
    ///
    /// Used by the control links that C reads via `dbGetLink` every
    /// process cycle — `SDIS`→`disa`, `SIML`→`simm`, `SELL`→`seln`, and
    /// `TSEL`'s `TSE` load. The pre-fix sites open-coded an
    /// `if let ParsedLink::Db` read, so a control link sourced over
    /// CA/PVA or given as a constant was silently ignored.
    pub(crate) fn read_link_value_no_process(
        &self,
        link: &crate::server::record::ParsedLink,
    ) -> Option<EpicsValue> {
        match link {
            crate::server::record::ParsedLink::None => None,
            crate::server::record::ParsedLink::Ca(ca) => self.resolve_external_pv(&ca.pv),
            crate::server::record::ParsedLink::Pva(name) => self.resolve_external_pv(name),
            // Per-link identity key, as in `read_link_value` above.
            crate::server::record::ParsedLink::PvaJson(j) => {
                self.resolve_external_pv(&j.link_identity_key())
            }
            crate::server::record::ParsedLink::Constant(_) => link.constant_value(),
            crate::server::record::ParsedLink::Db(db) => self.read_db_link_value(db),
            // Hardware links carry no generic readable value.
            crate::server::record::ParsedLink::Hw(_) => None,
            crate::server::record::ParsedLink::Calc(calc) => self.evaluate_calc_link(calc),
            crate::server::record::ParsedLink::State(st) => Some(self.read_state_link(st)),
        }
    }

    /// The record name a `time` argument's timestamp comes from, or `None`
    /// when that slot holds a numeric literal (nothing to read a time from).
    pub(super) fn calc_time_source_record(arg: &crate::server::record::CalcArg) -> Option<String> {
        use crate::server::record::{CalcArg, ParsedLink};
        let CalcArg::Link(link) = arg else {
            return None;
        };
        let name = match link.as_ref() {
            // Strip the `.FIELD` suffix — and any channel filter — to land
            // on the record name.
            ParsedLink::Db(db) => db.target().record,
            ParsedLink::Ca(ca) => ca.pv.clone(),
            ParsedLink::Pva(pv) => pv.clone(),
            ParsedLink::PvaJson(j) => j.pv.clone(),
            // A state link names no record, so it has no timestamp to adopt
            // — its lset implements no `getTimestampTag`.
            ParsedLink::None
            | ParsedLink::Constant(_)
            | ParsedLink::Hw(_)
            | ParsedLink::Calc(_)
            | ParsedLink::State(_) => return None,
        };
        Some(
            name.rsplit_once('.')
                .map(|(r, _)| r.to_string())
                .unwrap_or(name),
        )
    }

    /// lnkCalc evaluation: resolve each input, bind the calc engine's
    /// variable slots by position, run `expr`, return the result as
    /// `EpicsValue::Double`. Returns `None` if any input fetch fails, expr
    /// compile fails, or eval fails — the caller treats the link as
    /// unresolvable.
    pub fn evaluate_calc_link(&self, calc: &crate::server::record::CalcLink) -> Option<EpicsValue> {
        use crate::calc::engine::{CALC_NARGS, NumericInputs};
        use crate::server::record::CalcArg;
        // The parser already refuses an over-long `args`; a link built
        // in-process could still carry one, and C's limit is a refusal
        // everywhere it appears (`lnkCalc.c:135-139`).
        if calc.args.len() > CALC_NARGS {
            return None;
        }
        let mut vars = [0.0f64; CALC_NARGS];
        for (i, arg) in calc.args.iter().enumerate() {
            vars[i] = match arg {
                // Nothing reads over a literal: C's matching `inp[i]` is a
                // zeroed CONSTANT link and `dbGetLink` on it delivers
                // nothing (`dbConstLink.c:219-225`).
                CalcArg::Literal(n) => *n,
                // C `dbGetLink(child, DBR_DOUBLE, &clink->arg[i], …)`
                // (`lnkCalc.c:585`) — a read that never processes the
                // target, which is what `read_link_value_no_process` owns.
                // It also carries the locality split each input needs, since
                // every lnkCalc input is its own `dbInitLink` link and a
                // non-local one is a CA link.
                CalcArg::Link(link) => self.read_link_value_no_process(link)?.get_convert_f64()?,
            };
        }
        let compiled = crate::calc::compile(&calc.expr).ok()?;
        let mut inputs = NumericInputs::with_vars(vars);
        let result = crate::calc::eval(&compiled, &mut inputs).ok()?;
        Some(EpicsValue::Double(result))
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
    pub(crate) fn read_link_with_alarm(
        &self,
        link: &crate::server::record::ParsedLink,
    ) -> (crate::server::recgbl::simm::LinkFetch, Option<LinkAlarm>) {
        use crate::server::recgbl::simm::LinkFetch;
        let (value, alarm) = self.read_link_value_and_alarm(link);
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
        (fetch, alarm)
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
    ///   the remote stat + message. Constant/Hw/Calc links inherit nothing.
    pub(crate) fn input_link_inheritance(
        &self,
        reader_name: &str,
        link: &crate::server::record::ParsedLink,
        alarm: Option<LinkAlarm>,
    ) -> Option<(crate::server::record::MonitorSwitch, LinkAlarm)> {
        let alarm = alarm?;
        match link {
            crate::server::record::ParsedLink::Db(db) => {
                let record = db.target().record;
                let target = self
                    .resolve_alias(&record)
                    .unwrap_or_else(|| record.clone());
                let reader = self
                    .resolve_alias(reader_name)
                    .unwrap_or_else(|| reader_name.to_string());
                if target == reader {
                    return None;
                }
                Some((db.monitor_switch, alarm))
            }
            crate::server::record::ParsedLink::Ca(ca) => Some((ca.monitor_switch, alarm)),
            crate::server::record::ParsedLink::Pva(_)
            | crate::server::record::ParsedLink::PvaJson(_) => {
                Some((crate::server::record::MonitorSwitch::MaximizeStatus, alarm))
            }
            _ => None,
        }
    }

    /// The raw value+alarm read behind [`Self::read_link_with_alarm`]. Never
    /// call this directly from a process path — it cannot tell "constant" from
    /// "failed"; that is the classifier's job.
    fn read_link_value_and_alarm(
        &self,
        link: &crate::server::record::ParsedLink,
    ) -> (Option<EpicsValue>, Option<LinkAlarm>) {
        match link {
            crate::server::record::ParsedLink::Db(db) => {
                let target = db.target();
                let pv_name = local_pv_name(&target);
                // C `dbInitLink` locality (`dbLink.c:118-130`): a target
                // record present in this IOC is a DB link read from the
                // local database; a non-local target is a CA link, so its
                // value and raw remote alarm come from the external
                // resolver — identical to the `Ca`/`Pva` arm below. The
                // external name keeps the filter, the local one never has
                // it: `db.pvname()` is C's verbatim `pvname`.
                if !self.has_name_no_resolve(&target.record) {
                    let external = db.pvname();
                    return (
                        self.resolve_external_pv(&external),
                        self.external_link_alarm(&external),
                    );
                }
                let value = self.get_pv(&pv_name).ok().map(|v| {
                    match target.json_suffix.as_deref() {
                        // C `dbDbGetValue`'s filter arm, and its NULL-log
                        // fallback to the raw field (`dbDbLink.c:206-219`).
                        Some(suffix) => {
                            crate::server::database::filters::parse_filter_chain(suffix)
                                .apply_to_read_value(v.clone())
                                .unwrap_or(v)
                        }
                        None => v,
                    }
                });
                // Read source record's alarm state — alias-aware
                // (epics-base PR #336) so a link target spelled with
                // an alias still propagates MS/NMS alarm correctly.
                let alarm = if let Some(rec) = self.get_record(&target.record) {
                    let inst = rec.read();
                    Some(LinkAlarm::committed(&inst.common))
                } else {
                    None
                };
                (value, alarm)
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
            crate::server::record::ParsedLink::Pva(_)
            | crate::server::record::ParsedLink::PvaJson(_)
            | crate::server::record::ParsedLink::Ca(_) => {
                let name = link
                    .external_pv_name()
                    .expect("Ca/Pva/PvaJson link carries a PV name");
                let value = self.resolve_external_pv(&name);
                let alarm = self.external_link_alarm(&name);
                (value, alarm)
            }
            // A `lnkCalc` link computes its value from its own inputs; the
            // jlink has no source record whose alarm could be inherited
            // (`lnkCalc`'s lset implements no `getAlarm`), so it reads as a
            // value with no alarm.
            crate::server::record::ParsedLink::Calc(calc) => (self.evaluate_calc_link(calc), None),
            // `lnkState`'s lset implements no `getAlarm`, so a state link
            // reads as a value with no alarm to inherit.
            crate::server::record::ParsedLink::State(st) => (Some(self.read_state_link(st)), None),
            // Hardware links carry no generic readable value; `None` links
            // deliver nothing.
            crate::server::record::ParsedLink::Hw(_) | crate::server::record::ParsedLink::None => {
                (None, None)
            }
        }
    }

    /// Every registered lset, as a snapshot.
    ///
    /// The bare-name link paths try each lset in turn, and every lset call is
    /// now an await. Snapshotting the registry (rather than iterating it under
    /// its read guard) keeps the registry lock off the await path, so an lset
    /// that registers or unregisters another while resolving cannot deadlock
    /// against the caller.
    fn registered_link_sets(&self) -> Vec<crate::server::database::DynLinkSet> {
        let registry = self.inner.link_sets.load();
        registry
            .schemes()
            .iter()
            .filter_map(|s| registry.get(s))
            .collect()
    }

    /// Latched upstream timestamp from the lset, when the
    /// link is configured with `time=true`. The lset gates internally
    /// (returning `None` for links without the `time` option), so a
    /// `Some` here is the authoritative remote timestamp the
    /// processing path should adopt into the owning record's
    /// `common.time` and `common.utag`. Mirrors pvxs
    /// `pvxs/ioc/pvalink_lset.cpp:577-593`.
    ///
    /// Returns `(seconds_since_epoch, nanoseconds, userTag)` exactly as
    /// the lset reports them; the caller folds the time into the
    /// record's `SystemTime` via `UNIX_EPOCH + Duration::new(...)` and
    /// adopts the `userTag` into `common.utag`. The `userTag` is the
    /// remote `timeStamp.userTag` widened without sign extension, or `0`
    /// when the source carries none.
    pub(crate) fn external_link_time(&self, name: &str) -> Option<(i64, i32, u64)> {
        let (scheme, body) = if let Some(rest) = name.strip_prefix("pva://") {
            ("pva", rest)
        } else if let Some(rest) = name.strip_prefix("ca://") {
            ("ca", rest)
        } else {
            // Bare name — try every registered lset.
            for lset in self.registered_link_sets() {
                if let Some(ts) = lset.time_stamp(name) {
                    return Some(ts);
                }
            }
            return None;
        };
        let lset = self.inner.link_sets.load().get(scheme)?;
        lset.time_stamp(body)
    }

    /// Build a [`LinkAlarm`] from the registered lset's alarm
    /// accessors for an external (`pva://` / `ca://`) link, or `None`
    /// when no lset is registered or the lset reports no alarm.
    ///
    /// The lset's `alarm_severity` is the gated severity (see
    /// [`crate::server::database::LinkSet::alarm_severity`]); when it
    /// is `Some`, the `stat` is `LINK_ALARM` and the message comes
    /// from `alarm_message`.
    fn external_link_alarm(&self, name: &str) -> Option<LinkAlarm> {
        let (scheme, body) = if let Some(rest) = name.strip_prefix("pva://") {
            ("pva", rest)
        } else if let Some(rest) = name.strip_prefix("ca://") {
            ("ca", rest)
        } else {
            // Bare name — try every registered lset until one reports
            // a severity (mirrors `resolve_external_pv`'s bare path).
            for lset in self.registered_link_sets() {
                if let Some(sev) = lset.alarm_severity(name) {
                    return Some(LinkAlarm {
                        // prefer the remote STAT for MSS;
                        // fall back to LINK_ALARM when the lset has none.
                        stat: lset
                            .alarm_status(name)
                            .map(|s| s as u16)
                            .unwrap_or(crate::server::recgbl::alarm_status::LINK_ALARM),
                        sevr: crate::server::record::AlarmSeverity::from_u16(sev as u16),
                        amsg: lset.alarm_message(name).unwrap_or_default(),
                    });
                }
            }
            return None;
        };
        let lset = self.inner.link_sets.load().get(scheme)?;
        let sev = lset.alarm_severity(body)?;
        Some(LinkAlarm {
            // remote STAT for MSS, else LINK_ALARM.
            stat: lset
                .alarm_status(body)
                .map(|s| s as u16)
                .unwrap_or(crate::server::recgbl::alarm_status::LINK_ALARM),
            sevr: crate::server::record::AlarmSeverity::from_u16(sev as u16),
            amsg: lset.alarm_message(body).unwrap_or_default(),
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
        name: &str,
    ) -> Option<crate::server::database::RemoteAlarm> {
        let (scheme, body) = if let Some(rest) = name.strip_prefix("pva://") {
            ("pva", rest)
        } else if let Some(rest) = name.strip_prefix("ca://") {
            ("ca", rest)
        } else {
            for lset in self.registered_link_sets() {
                if let Some(snap) = lset.remote_alarm(name) {
                    return Some(snap);
                }
            }
            return None;
        };
        let lset = self.inner.link_sets.load().get(scheme)?;
        lset.remote_alarm(body)
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
        name: &str,
    ) -> Option<crate::server::database::LinkMetadata> {
        let (scheme, body) = if let Some(rest) = name.strip_prefix("pva://") {
            ("pva", rest)
        } else if let Some(rest) = name.strip_prefix("ca://") {
            ("ca", rest)
        } else {
            for lset in self.registered_link_sets() {
                if let Some(meta) = lset.link_metadata(name) {
                    return Some(meta);
                }
            }
            return None;
        };
        let lset = self.inner.link_sets.load().get(scheme)?;
        lset.link_metadata(body)
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
        visited: &mut HashSet<String>,
    ) -> Option<crate::server::database::LinkMetadata> {
        use crate::server::record::ParsedLink;
        match link {
            ParsedLink::Db(db) => self.db_link_metadata(db, visited),
            ParsedLink::Ca(_) | ParsedLink::Pva(_) | ParsedLink::PvaJson(_) => {
                let name = link
                    .external_pv_name()
                    .expect("Ca/Pva/PvaJson link carries a PV name");
                self.external_link_metadata(&name)
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
        visited: &mut HashSet<String>,
    ) -> Option<crate::server::database::LinkMetadata> {
        let key = db.pvname();
        // C `if (!(mutable_plink->flags & DBLINK_FLAG_VISITED))`
        // (`dbDbLink.c:253`): a re-entered link returns the `S_dbLib_badLink`
        // initialiser without touching the buffer.
        if !visited.insert(key.clone()) {
            return None;
        }
        let meta = self.db_target_metadata(db, visited);
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
    /// split `read_target_value` makes for the value path.
    fn db_target_metadata(
        &self,
        db: &crate::server::record::DbLink,
        visited: &mut HashSet<String>,
    ) -> Option<crate::server::database::LinkMetadata> {
        let target = db.target();
        if !self.has_name_no_resolve(&target.record) {
            return self.external_link_metadata(&db.pvname());
        }
        let record = self.get_record(&target.record)?;
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
        let snapshot =
            self.channel_snapshot_for_field_guarded(&record, &target.field, false, visited)?;
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
            enum_choices: snapshot.enums.as_ref().map(|e| {
                e.strings
                    .iter()
                    .map(|s| s.as_str_lossy().into_owned())
                    .collect()
            }),
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
    /// The caller's `visited` set and `depth` are threaded through into
    /// the source's processing cycle — NOT a fresh set / depth 0. This
    /// is required for the cycle guard to span the PP hop: in C,
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
    /// `visited`/`depth` the same way (processing.rs FLNK dispatch).
    pub(crate) fn process_passive_db_source(
        &self,
        db: &crate::server::record::DbLink,
        visited: &mut HashSet<String>,
        depth: usize,
    ) {
        if db.policy != crate::server::record::LinkProcessPolicy::ProcessPassive {
            return;
        }
        let record = db.target().record;
        if let Some(src) = self.get_record(&record) {
            let is_passive = src.read().common.scan == crate::server::record::ScanType::Passive;
            if is_passive {
                // recursive INP-link source processing within
                // one chain — gate held by the foreign entry record.
                let _ = self.process_record_with_links_recursive(&record, visited, depth + 1);
            }
        }
    }

    /// Read a value from a parsed link for INP (only reads DB links when soft channel).
    ///
    /// `visited` / `depth` are the caller's processing-chain state — a PP
    /// input link's source is processed *within* that same chain so the
    /// `visited` cycle guard spans the PP hop (see
    /// `Self::process_passive_db_source`).
    pub fn read_link_value_soft(
        &self,
        link: &crate::server::record::ParsedLink,
        is_soft: bool,
        visited: &mut HashSet<String>,
        depth: usize,
    ) -> Option<EpicsValue> {
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
            crate::server::record::ParsedLink::Constant(_) => None,
            crate::server::record::ParsedLink::Db(db) if is_soft => {
                // PP: process source record if Passive before reading
                self.process_passive_db_source(db, visited, depth);
                self.read_db_link_value(db)
            }
            crate::server::record::ParsedLink::Ca(_)
            | crate::server::record::ParsedLink::Pva(_)
            | crate::server::record::ParsedLink::PvaJson(_)
                if is_soft =>
            {
                let name = link
                    .external_pv_name()
                    .expect("Ca/Pva/PvaJson link carries a PV name");
                self.resolve_external_pv(&name)
            }
            // lnkCalc evaluates regardless of `is_soft` — the input
            // PVs may themselves be local DB targets (which need the
            // soft path) or remote CA/PVA, but the calc evaluation
            // is uniform either way.
            crate::server::record::ParsedLink::Calc(calc) => self.evaluate_calc_link(calc),
            // A state link answers whatever the DTYP, for the same reason a
            // calc link does: `lnkState`'s own lset implements `getValue`
            // (`lnkState.c:139-151`), so there is no device support to route
            // around and no target record to process first.
            crate::server::record::ParsedLink::State(st) => Some(self.read_state_link(st)),
            _ => None,
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
    pub(crate) fn process_target(
        &self,
        target_name: &str,
        gate: ProcessTargetGate,
        src_putf: bool,
        src_notify: Option<&Arc<NotifyWaitSet>>,
        visited: &mut HashSet<String>,
        depth: usize,
    ) {
        let Some(target_rec) = self.get_record(target_name) else {
            return;
        };
        {
            let mut tg = target_rec.write();
            // The gate. A target that does not pass it never reaches
            // `processTarget`, so it gets no PUTF, no RPRO, and no put-notify
            // join — the join in particular would `enter` the wait-set without
            // ever `leave`ing it.
            if !gate.admits(tg.common.scan) {
                return;
            }
            let pact = tg.is_processing();
            if !pact {
                tg.common.putf = src_putf;
                tg.join_put_notify(src_notify);
            } else if src_putf && !visited.contains(target_name) {
                tg.common.rpro = 1;
                tg.common.putf = false;
            }
        }
        // Recursive target processing within one chain — the gate is already
        // held by the foreign entry record. Unconditional, as C's
        // `dbProcess(pdst)` is: an active target is refused inside
        // `process_record_with_links_body`'s PACT arm, the single owner of
        // that decision, and the refusal is counted there.
        let _ = self.process_record_with_links_recursive(target_name, visited, depth + 1);
    }

    /// Write a value through a DbLink, optionally processing the target if PP and Passive.
    ///
    /// Returns `true` when the write failed — C `dbPutLink` status `!= 0`:
    /// the local `dbPut` was rejected (conversion error, missing field) or
    /// the non-local external write returned `Err`. Callers that mirror a
    /// record's `dbPutLink` status (e.g. dfanout `push_values` raising
    /// `LINK_ALARM/MAJOR`, dfanoutRecord.c:312) fold this into the source's
    /// alarm; the fanout/seq dispatch paths ignore it.
    ///
    /// `src_putf` carries the source record's `PUTF` bit so the target inherits
    /// it the same way C `dbDbLink.c::processTarget` propagates it (lines 470-498):
    ///
    /// - target not pact: `target.putf = src_putf` (normal propagation),
    /// - target pact AND `src_putf` AND target not on current process chain:
    ///   `target.rpro = true`, `target.putf = false` so the in-flight cycle
    ///   reprocesses on completion attributing the put to the originator,
    /// - otherwise: no PUTF change (target is either being processed
    ///   recursively by us, or wasn't triggered by a dbPutField).
    ///
    /// Without this, a CA WRITE_NOTIFY landing on an upstream calc/seq/dfanout
    /// that fanned out via DB OUT links would see `target.putf = 0` on every
    /// downstream record — breaking dbNotify completion attribution and any
    /// device-support code that uses PUTF to distinguish operator-driven from
    /// scan-driven processing.
    pub(crate) fn write_db_link_value(
        &self,
        link: &crate::server::record::DbLink,
        value: EpicsValue,
        src: OutLinkSrc<'_>,
        visited: &mut HashSet<String>,
        depth: usize,
    ) -> bool {
        let target = link.target();
        let target_name = local_pv_name(&target);
        // C `dbInitLink` locality (`dbLink.c:118-130`): a target record
        // not present in this IOC is a CA link, so its write is routed
        // through the external put path (`dbCaPutLink`), not a local
        // `dbPut`. The alarm-inheritance / PUTF / `processTarget`
        // machinery below is the local-DB `dbDbPutValue` body
        // (dbDbLink.c:372-393), which `dbCaPutLink` performs none of —
        // so a non-local target returns right after the remote write.
        // OUTPUT-side twin of the `read_db_link_value` locality fallback.
        if !self.has_name_no_resolve(&target.record) {
            let external = link.pvname();
            if let Err(e) = self.write_external_pv(&external, value, src.notify) {
                eprintln!("OUT-link write to external PV '{external}' failed: {e}");
                return true;
            }
            return false;
        }
        // an OUT-link write-back is an internal step of the
        // processing chain that already holds the entry record's
        // advisory write gate (`dbScanLock` analogue). It must use the
        // `_already_locked` write so it does not re-acquire a gate: a
        // self-referencing OUT link (`SELF PP`) would otherwise
        // dead-lock on the entry record's own non-reentrant gate. C
        // `dbDbPutValue` writes the OUT-link target under the same
        // lock set the chain already owns.
        // No put-notify restart is armed by this write, whether or not the
        // `PP` / `.PROC` branch below drives a cycle: C's owner is the cycle
        // tail (`recGblFwdLink` → `dbNotifyCompletion`), so a target whose park
        // this releases replays on its next cycle and an NPP link — `gate` is
        // `None` below — leaves it parked, exactly as C does.
        let put_result = self.put_pv_already_locked(&target_name, value);

        // C `dbDbPutValue` (dbDbLink.c:382-383) folds the SOURCE
        // record's alarm into the destination via `recGblInheritSevrMsg`,
        // AFTER the `dbPut` and BEFORE `processTarget`. The fields C reads
        // there are `psrce->nsta/nsev/namsg` — the source's PENDING alarm,
        // because the put runs inside the source's `process()`, before its
        // `recGblResetAlarms`. Every OUT-put site in this port drives its
        // writes in that same pre-commit window and hands them
        // [`LinkAlarm::pending`]; a committed snapshot would carry the
        // PREVIOUS cycle's severity. The inherited
        // severity lands in the dest's PENDING nsev/nsta(/namsg for MSS);
        // the dest commits it on its next `rec_gbl_reset_alarms` — its
        // own process cycle, reached below for a `.PROC`/`PP` link, or a
        // later independent scan otherwise. NMS (the common case) skips
        // the dest lookup/lock entirely.
        if link.monitor_switch != crate::server::record::MonitorSwitch::NoMaximize {
            if let Some(target_rec) = self.get_record(&target.record) {
                let mut tg = target_rec.write();
                inherit_sevr_msg(&mut tg.common, link.monitor_switch, src.alarm);
            }
        }

        // C `dbDbPutValue` (dbDbLink.c:384-385) returns the put status
        // immediately after the alarm inheritance and BEFORE the
        // `.PROC`/`PP` `processTarget` branch: only a successful write
        // reaches target processing. A failed OUT-link write (missing
        // field, record put rejection) must therefore NOT trigger the
        // target's process cycle, which would otherwise run the target
        // on its stale field value and diverge from C on side effects,
        // FLNK, alarms, and put-notify completion ordering. The alarm
        // inheritance above already ran (C folds it regardless of
        // status), matching the C ordering exactly.
        //
        // An empty array into a scalar field is NOT such a failure: C
        // `dbPut` accepts it, raises LINK/INVALID on the destination and
        // returns 0 (`dbAccess.c:1370-1372`), so a `PP` link still
        // processes its target — see `field_io::PutRequest`.
        if put_result.is_err() {
            return true;
        }

        // C `dbDbPutValue` (`dbDbLink.c:387-390`) processes the target when the
        // destination field is `.PROC` **or** the link carries `pvlOptPP` (an
        // explicit ` PP` token → `ProcessPassive`) and the target is Passive.
        // The two arms are different gates, not one: the `.PROC` arm is
        // independent of both the PP flag and the target's SCAN (R18-94). It is
        // therefore checked here in the write path rather than encoded as a
        // parse-time policy: a modifier-less link defaults to `NoProcess`
        // uniformly (INPUT and OUTPUT alike), and writing into a record's
        // `.PROC` field still forces a process.
        let gate = if target.field == "PROC" {
            Some(ProcessTargetGate::ProcField)
        } else if link.policy == crate::server::record::LinkProcessPolicy::ProcessPassive {
            Some(ProcessTargetGate::ScanPassive)
        } else {
            None
        };
        if let Some(gate) = gate {
            // Through the single `processTarget` owner, which holds the gate.
            // Alias-aware: `process_target` resolves the name, as
            // `process_record_with_links` does at entry.
            self.process_target(&target.record, gate, src.putf, src.notify, visited, depth);
        }
        // Successful local write (C `dbPutLink` status 0).
        false
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
        name: &'a str,
    ) -> Result<(super::link_put_queue::LinkTarget, &'a str), String> {
        use super::link_put_queue::LinkTarget;
        use super::link_set::PutAdmission;

        let (target, body, lsets) = if let Some(rest) = name.strip_prefix("pva://") {
            let lset = self
                .inner
                .link_sets
                .load()
                .get("pva")
                .ok_or_else(|| format!("no 'pva' link set registered for '{name}'"))?;
            (LinkTarget::Scheme("pva".to_string()), rest, vec![lset])
        } else if let Some(rest) = name.strip_prefix("ca://") {
            let lset = self
                .inner
                .link_sets
                .load()
                .get("ca")
                .ok_or_else(|| format!("no 'ca' link set registered for '{name}'"))?;
            (LinkTarget::Scheme("ca".to_string()), rest, vec![lset])
        } else {
            // Bare name — every registered lset is a candidate, first
            // accepting write wins (mirrors `resolve_external_pv`'s
            // bare-name path).
            let lsets = self.registered_link_sets();
            if lsets.is_empty() {
                return Err(format!("no link set registered for external link '{name}'"));
            }
            (LinkTarget::Any, name, lsets)
        };

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
            return Err(format!(
                "external link '{name}' refused the write: this database was \
                 built with no tokio runtime, so no link set can reach the \
                 network"
            ));
        }

        // A link that is known-down refuses the put and stages nothing, so the
        // record alarms in this cycle. `put_admission` is answered from cached
        // state — this is the only lset call left inside the record's advisory
        // write gate, and it does no I/O.
        let mut admission = PutAdmission::Refused;
        for lset in &lsets {
            match lset.put_admission(body) {
                PutAdmission::Connected => {
                    admission = PutAdmission::Connected;
                    break;
                }
                // Unopened outranks Refused: the write must still be
                // staged so the lset's lazy open runs (see `PutAdmission`).
                PutAdmission::Unopened => admission = PutAdmission::Unopened,
                PutAdmission::Refused => {}
            }
        }
        if admission == PutAdmission::Refused {
            return Err(format!(
                "external link '{name}' refused the write: not connected, or \
                 connected without write access (db/dbCa.c:529-532 \
                 (dbCaPutLinkCallback))"
            ));
        }
        Ok((target, body))
    }

    /// Ask [`Self::external_put_gate`] on its own, staging nothing: `Ok` iff a
    /// put to `name` would be issued right now.
    pub(crate) fn external_put_admitted(&self, name: &str) -> Result<(), String> {
        self.external_put_gate(name).map(|_| ())
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
    /// scheme). For a bare name every registered lset is tried in
    /// turn — the first whose `put_value` succeeds wins.
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
    /// * `Ok(())` for [`LinkPutOp::Plain`] — the write is staged. It is
    ///   **not** yet on the wire. This is the fire-and-forget flavour, C's
    ///   `CA_PUT` / `ca_array_put` (`dbCa.c:1163-1166`), and it is the whole
    ///   point of the queue: record processing no longer suspends on a
    ///   `ca://`/`pva://` round trip inside the record's advisory write gate.
    /// * `Ok(())` for [`LinkPutOp::Async`] too — the completion flavour, C's
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
    /// `src_notify` is the source record's put-completion wait-set. Its
    /// presence is what selects [`LinkPutOp::Async`] over
    /// [`LinkPutOp::Plain`] — see [`Self::external_put_op`], the single
    /// owner of that mapping — and it is also the thing the completion is
    /// reported through, so the two cannot disagree.
    pub(crate) fn write_external_pv(
        &self,
        name: &str,
        value: EpicsValue,
        src_notify: Option<&Arc<NotifyWaitSet>>,
    ) -> Result<(), String> {
        let op = Self::external_put_op(src_notify);
        use super::link_put_queue::LinkKey;

        let (target, body) = self.external_put_gate(name)?;

        self.inner
            .link_puts
            .ensure_owner(std::sync::Arc::downgrade(&self.inner));
        let key = LinkKey {
            target,
            name: body.to_string(),
        };
        match self.inner.link_puts.stage_put(key, value, op) {
            // `dbCaPutLink`: staged, signalled, returned (`dbCa.c:593-595`).
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
                if let Some(waitset) = src_notify {
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
    /// must observe the effect of a [`LinkPutOp::Plain`] write, which by
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

    /// Fire a forward link (FLNK) whose target is an external
    /// (`pva://` / `ca://`) PV — the FWD-link counterpart of
    /// [`Self::write_external_pv`]. Resolves the scheme (or tries every
    /// registered lset for a bare name, first to accept wins, exactly as
    /// the OUT-write path does) and delegates to [`LinkSet::scan_forward`](super::LinkSet::scan_forward).
    ///
    /// Mirrors C `dbScanFwdLink` → `plink->lset->scanForward`
    /// (`dbLink.c:475-480`): the database hands the forward link to the
    /// link set, which (for pvalink) runs `pvaScanForward`. Returns the
    /// lset's `Err` unchanged so the caller can raise LINK/INVALID on the
    /// owning record (pvxs `recGblSetSevrMsg(LINK_ALARM, INVALID_ALARM)`).
    pub(crate) fn scan_forward_external_pv(&self, name: &str) -> Result<(), String> {
        let (scheme, body) = if let Some(rest) = name.strip_prefix("pva://") {
            ("pva", rest)
        } else if let Some(rest) = name.strip_prefix("ca://") {
            ("ca", rest)
        } else {
            // Bare name — try every registered lset in turn, first
            // accepting the forward wins (mirrors `write_external_pv`'s
            // bare-name path so FWD and OUT route a bare target alike).
            let lsets = self.registered_link_sets();
            if lsets.is_empty() {
                return Err(format!("no link set registered for forward link '{name}'"));
            }
            let mut last_err = String::new();
            for lset in lsets {
                match lset.scan_forward(name) {
                    Ok(()) => return Ok(()),
                    Err(e) => last_err = e,
                }
            }
            return Err(last_err);
        };
        let lset = self
            .inner
            .link_sets
            .load()
            .get(scheme)
            .ok_or_else(|| format!("no '{scheme}' link set registered for '{name}'"))?;
        lset.scan_forward(body)
    }

    /// Map a record's put-completion wait-set to the external-link put
    /// op. A write that originates inside a put-notify / blocking-put
    /// chain (the source record carries a completion wait-set) is
    /// delivered as [`LinkPutOp::Async`] (the pvxs `pvaPutValueAsync` /
    /// C `dbPutLinkAsync` path); a plain record-processing OUT write is
    /// [`LinkPutOp::Plain`]. Single owner of the notify→op mapping so
    /// the external-OUT dispatch sites cannot diverge.
    fn external_put_op(src_notify: Option<&Arc<NotifyWaitSet>>) -> LinkPutOp {
        if src_notify.is_some() {
            LinkPutOp::Async
        } else {
            LinkPutOp::Plain
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
        let external = |name: String| {
            match self.external_link_metadata(&name) {
                Some(m) => {
                    let field_type = m.dbf_type.map(link_dbf_to_field_type);
                    OutTarget {
                        field_type,
                        element_count: m.element_count.unwrap_or(1).max(1),
                        is_ca_link: true,
                        // A remote channel reports the DBR type its DBF class
                        // is served as (`dbCaGetLinkDBFtype` → the channel's
                        // type): a remote `DBF_MENU`/`DBF_DEVICE`/link field
                        // arrives as `DBR_ENUM`/`DBR_STRING`, so the two wire
                        // types ARE the string class over a CA link.
                        puts_as_string: matches!(
                            field_type,
                            Some(DbFieldType::String) | Some(DbFieldType::Enum)
                        ),
                    }
                }
                None => OutTarget {
                    is_ca_link: true,
                    ..OutTarget::UNRESOLVED
                },
            }
        };
        match link {
            crate::server::record::ParsedLink::Db(db) => {
                let addressed = db.target();
                if !self.has_name_no_resolve(&addressed.record) {
                    // Non-local ⇒ CA link in C (`dbInitLink` locality).
                    return external(db.pvname());
                }
                let Some(target) = self.get_record(&addressed.record) else {
                    return OutTarget::UNRESOLVED;
                };
                let guard = target.read();
                let field_type = crate::server::record::record_instance::declared_field_type_of(
                    guard.record.as_ref(),
                    &addressed.field,
                )
                .or_else(|| {
                    guard
                        .record
                        .get_field(&addressed.field)
                        .map(|v| v.db_field_type())
                });
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
                    element_count: element_count.max(1),
                    is_ca_link: false,
                    // C's `dbNameToAddr` gives the target field's DBF CLASS,
                    // which includes `DBF_MENU` / `DBF_DEVICE` — classes the
                    // port's DBR-typed `field_type` cannot name. The target
                    // record classifies its own field.
                    puts_as_string: guard.field_puts_as_string(&addressed.field),
                }
            }
            crate::server::record::ParsedLink::Ca(_)
            | crate::server::record::ParsedLink::Pva(_)
            | crate::server::record::ParsedLink::PvaJson(_) => {
                let name = link
                    .external_pv_name()
                    .expect("Ca/Pva/PvaJson link carries a PV name");
                external(name.to_string())
            }
            // C `lnkState_getDBFtype`/`lnkState_getElements`
            // (`lnkState.c:126-135`): one `DBF_SHORT` element, and never a
            // string.
            crate::server::record::ParsedLink::State(_) => OutTarget {
                field_type: Some(DbFieldType::Short),
                element_count: 1,
                is_ca_link: false,
                puts_as_string: false,
            },
            // Constant / Hw / Calc / None: not a writable target — the write
            // path no-ops, so the metadata is never used.
            _ => OutTarget::UNRESOLVED,
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
        rec: &Arc<parking_lot::RwLock<RecordInstance>>,
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
    pub(crate) fn dispatch_multi_output_values(
        &self,
        rec: &Arc<parking_lot::RwLock<RecordInstance>>,
        src: OutLinkSrc<'_>,
        skip_out: bool,
        visited: &mut HashSet<String>,
        depth: usize,
    ) {
        let pairs = {
            let instance = rec.read();
            // IVOA=Don't_drive veto (execOutput `nsev >= INVALID` →
            // Don't_drive `break`, sCalcoutRecord.c:794). `skip_out` is the
            // decision the cycle's single IVOA owner already made, on the
            // severity `checkAlarms` produced — re-deriving it here from
            // `nsev` would read an alarm the OUT write above may have raised
            // itself (a failed put's LINK_ALARM/INVALID), acting on a veto C
            // never applied.
            let links = if skip_out || multi_output_dispatch_owned(instance.record.record_type()) {
                &[][..]
            } else {
                instance.record.multi_output_links()
            };
            let mut pairs = Vec::new();
            for &(link_field, val_field) in links {
                let link_str = match instance.record.get_field(link_field) {
                    Some(EpicsValue::String(s)) => s,
                    _ => continue,
                };
                if link_str.is_empty() {
                    continue;
                }
                if let Some(val) = instance.record.get_field(val_field) {
                    pairs.push((link_field, link_str, val));
                }
            }
            pairs
        };
        for (link_field, link_str, val) in pairs {
            // `multi_output_links` carries record OUT links (scalcout /
            // acalcout `OUT` — `DBF_OUTLINK`) driven via `dbPutLink` →
            // `dbDbPutValue` (`dbDbLink.c:388`): a bare DB link is NPP, the
            // value is written but the target is NOT processed.
            // `parse_output_link_v2` applies that OUT-link-correct NPP default;
            // `parse_link_v2` would wrongly default a bare link to
            // ProcessPassive and re-process the target. An external
            // `ca://`/`pva://` OUT link routes through the link set's
            // `putValue` (C `dbLink.c::dbPutLink`, dbLink.c:434-448).
            let parsed =
                crate::server::record::parse_output_link_v2(link_str.as_str_lossy().as_ref());
            // Device-support write-buffer switch: the resolved target's DBF
            // type / element count decides which of the record's buffers C
            // would actually put (`devsCalcoutSoft.c:66-144`,
            // `devaCalcoutSoft.c:75-87`).
            let val = self.multi_out_buffer_choice(rec, link_field, &parsed, val);
            self.write_out_link_value(
                rec,
                &parsed,
                val,
                OutLinkSrc {
                    field: link_field,
                    ..src
                },
                visited,
                depth,
            );
        }
    }

    /// **The put owner** — C `dbLink.c::dbPutLink` (434-448). Writes a value
    /// through a parsed OUT link, dispatching DB links to
    /// [`Self::write_db_link_value`] and external (`ca://`/`pva://`) links to
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
    /// This is also the OUTPUT-side counterpart of [`Self::read_link_value`]'s
    /// scheme dispatch: the OUT-link write stage in `processing.rs`
    /// must route a `ParsedLink::Ca`/`Pva` through the link set, not
    /// only handle `ParsedLink::Db`.
    ///
    /// `Constant`/`Hw`/`Calc`/`None` OUT links are not writable
    /// targets and are silently skipped (C `dbPutLink` returns
    /// `S_db_noLSET` for a link with no lset — the same no-op, and C does NOT
    /// alarm on `S_db_noLSET`: `dbGetLink` explicitly maps it to a plain `-1`
    /// with no `setLinkAlarm`).
    ///
    /// Returns C's `dbPutLink` status as a bool — `true` when the put failed.
    /// A caller needs it only to reproduce a record-specific alarm C raises ON
    /// TOP of the put's own (dfanout's `LINK_ALARM`/`MAJOR`); the INVALID that
    /// every failing put owes the record is already raised here.
    pub(crate) fn write_out_link_value(
        &self,
        src_rec: &Arc<parking_lot::RwLock<RecordInstance>>,
        link: &crate::server::record::ParsedLink,
        value: EpicsValue,
        src: OutLinkSrc<'_>,
        visited: &mut HashSet<String>,
        depth: usize,
    ) -> bool {
        let failed = match link {
            crate::server::record::ParsedLink::Db(db) => {
                self.write_db_link_value(db, value, src, visited, depth)
            }
            crate::server::record::ParsedLink::Ca(_)
            | crate::server::record::ParsedLink::Pva(_)
            | crate::server::record::ParsedLink::PvaJson(_) => {
                let name = link
                    .external_pv_name()
                    .expect("Ca/Pva/PvaJson link carries a PV name");
                match self.write_external_pv(&name, value, src.notify) {
                    Ok(()) => false,
                    Err(e) => {
                        eprintln!("OUT-link write to external PV '{name}' failed: {e}");
                        true
                    }
                }
            }
            // C `lnkState_putValue` (`lnkState.c:153-203`) drives the named
            // bit; a DBR class it has no arm for is `S_db_badDbrtype`, which
            // `dbPutLink` reports as a failed put.
            crate::server::record::ParsedLink::State(st) => {
                match crate::server::record::StateLink::truth_of(&value) {
                    Some(truth) => {
                        self.db_state(&st.name).set(st.write(truth));
                        false
                    }
                    None => {
                        eprintln!(
                            "OUT-link write to state '{}' failed: unsupported value type",
                            st.name
                        );
                        true
                    }
                }
            }
            // Constant / Hw / Calc / None are not writable OUT-link
            // targets — no-op (C `dbPutLink` → `S_db_noLSET`).
            _ => false,
        };
        if failed {
            let mut inst = src_rec.write();
            crate::server::recgbl::rec_gbl_set_link_alarm(&mut inst.common, src.field);
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
    /// C `lnkState_open` (`lnkState.c:110-116`) calls `dbStateCreate` once
    /// when the link opens at `iocInit`, so the state exists from then on
    /// whether or not anything ever sets it; get-or-create at first use is
    /// that rule with the same observable result. The registry is the
    /// process-wide one the `sync` channel filter and the `Db State` device
    /// support already share, so all three see one bit per name.
    fn db_state(&self, name: &str) -> Arc<crate::server::database::filters::sync::DbState> {
        crate::server::database::filters::sync::db_state_registry().get_or_create(name)
    }

    /// Read a record String field, defaulting to empty.
    fn field_str(instance: &RecordInstance, field: &str) -> String {
        match instance.record.get_field(field) {
            Some(EpicsValue::String(s)) => s.as_str_lossy().into_owned(),
            _ => String::new(),
        }
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

    /// Apply a SELM-resolved out-of-range alarm to the record.
    ///
    /// C raises this alarm inside `process()` (before `recGblResetAlarms`)
    /// via `recGblSetSevr(prec, SOFT_ALARM, INVALID_ALARM)`. The Rust
    /// multi-output dispatch runs after the record's own alarm reset,
    /// so we apply the severity directly to `common.sevr/stat`, refresh
    /// the live STAT/SEVR fields, and post the monitor — matching the
    /// observable end state (record reads INVALID/SOFT_ALARM, a
    /// `DBE_ALARM` subscriber on STAT/SEVR is notified).
    fn apply_selm_alarm(
        rec: &Arc<parking_lot::RwLock<RecordInstance>>,
        alarm: Option<(u16, AlarmSeverity)>,
    ) {
        let Some((stat, sevr)) = alarm else {
            return;
        };
        let posted = {
            let mut inst = rec.write();
            // Raise-only, mirroring recGblSetSevr.
            if (sevr as u16) > (inst.common.sevr as u16) {
                inst.common.sevr = sevr;
                inst.common.stat = stat;
                true
            } else {
                false
            }
        };
        if posted {
            // Write guard: a value-class post advances the record's
            // already-published state (`RecordInstance::record_value_post`),
            // so posting is a `&mut` operation.
            let mut inst = rec.write();
            inst.notify_field("SEVR", crate::server::recgbl::EventMask::ALARM);
            inst.notify_field("STAT", crate::server::recgbl::EventMask::VALUE);
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
        rec: &Arc<parking_lot::RwLock<RecordInstance>>,
        phase: SellPhase,
    ) {
        let sell = {
            let instance = rec.read();
            match (instance.record.record_type(), phase) {
                ("dfanout", SellPhase::BeforeAlarms) | ("fanout", SellPhase::OutputDispatch) => {
                    Some(Self::field_str(&instance, "SELL"))
                }
                // seqSELM_All == 0 (seqRecord.dbd menu(seqSELM) order
                // All/Specified/Mask).
                ("seq", SellPhase::OutputDispatch) if Self::field_i16(&instance, "SELM") != 0 => {
                    Some(Self::field_str(&instance, "SELL"))
                }
                _ => None,
            }
        };
        let Some(sell) = sell else { return };
        if sell.is_empty() {
            return;
        }
        // C reads SELL via `dbGetLink` for any link type; the pre-fix port only
        // read a `ParsedLink::Db` SELL, so a SELL sourced over CA/PVA or given
        // as a constant never updated SELN.
        let parsed = crate::server::record::parse_link_v2(&sell);
        // Through the one classifier: a CONSTANT SELL delivers NOTHING here
        // (`dbConstGetValue`) — its value reached SELN once, at init
        // (`fanoutRecord.c:88`, `dfanoutRecord.c:102`, `seqRecord.c:121`
        // `recGblInitConstantLink(&sell, DBF_USHORT, &seln)`), so a
        // `caput REC.SELN` STAYS put.
        if let Some(val) = self.db_get_link(rec, "SELL", &parsed).value() {
            // The conversion routine is chosen by the SOURCE type — an integer
            // source wraps mod 2^16, a float source is UB. `dbr_ushort_cast`
            // owns that split. SELN is a `DBF_USHORT` (u16) field either way,
            // and `select_link_indices_ex` consumes it as `u16` — never as a
            // signed value that could clamp to 0.
            let seln = dbr_ushort_cast(&val);
            let mut instance = rec.write();
            let _ = instance.record.put_field("SELN", EpicsValue::UShort(seln));
        }
    }

    /// Multi-output dispatch for fanout, dfanout, seq record types.
    ///
    /// The per-record payload is a typed [`MultiOut`] — seq / sseq
    /// groups are kept as struct fields, NOT `\0`-packed strings
    /// (the pre-fix encoding could mis-split a link string that
    /// happened to contain an embedded NUL).
    ///
    /// `phase` selects the dispatch phase — see [`MultiOutPhase`]. A record
    /// whose links do not belong to the calling phase is skipped, so each
    /// type dispatches exactly once per cycle.
    ///
    /// In the [`MultiOutPhase::Output`] phase this returns the pending alarm
    /// `(stat, sevr)` the C record raises alongside its puts — a failed
    /// `dbPutLink` (dfanout's own `LINK_ALARM/MAJOR`,
    /// `dfanoutRecord.c:311-312`) or a `SELN` out of range
    /// (`SOFT_ALARM/INVALID`, `dfanoutRecord.c:317` / `seqRecord.c:157`) —
    /// for the caller to fold into `nsev` before the alarm commit. The
    /// [`MultiOutPhase::ForwardLink`] phase returns `None`: a fanout raises
    /// its range alarm directly into the already-committed SEVR.
    pub(crate) fn dispatch_multi_output(
        &self,
        rec: &Arc<parking_lot::RwLock<RecordInstance>>,
        phase: MultiOutPhase,
        visited: &mut HashSet<String>,
        depth: usize,
    ) -> MultiOutDispatch {
        // Phase gate, keyed on what the record's links ARE (see
        // `multi_out_phase_of`), not on which argument the caller passed.
        let record_type = rec.read().record.record_type().to_string();
        let is_value_phase = matches!(phase, MultiOutPhase::Output { .. });
        if matches!(multi_out_phase_of(&record_type), MultiOutPhaseKind::Output) != is_value_phase {
            return MultiOutDispatch::default();
        }

        // Snapshot the source record's PUTF bit + put-notify wait-set so
        // every write_db_link_value call below propagates them to its
        // target — C `dbDbLink.c::processTarget` PUTF and `dbNotifyAdd`
        // wait-set invariants (see write_db_link_value doc). The PENDING
        // alarm travels the same way for `recGblInheritSevrMsg` MS-class
        // propagation into each OUT-link target: the OUTn/LNKn puts precede
        // the source's `recGblResetAlarms`, so C reads `psrce->nsta/nsev`
        // here ([`LinkAlarm::pending`], dbDbLink.c:382-383).
        let (src_putf, src_notify, src_alarm) = {
            let guard = rec.read();
            (
                guard.common.putf,
                guard.notify.clone(),
                LinkAlarm::pending(&guard.common),
            )
        };
        // One snapshot threaded to every OUT-link write below; each arm
        // overrides `field` with the link field it is driving (OUTn / LNKn),
        // which C `setLinkAlarm` reports in the record's AMSG.
        let out_src = OutLinkSrc {
            putf: src_putf,
            notify: src_notify.as_ref(),
            alarm: &src_alarm,
            field: "",
        };

        self.read_sell_into_seln(rec, SellPhase::OutputDispatch);

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
                    let links: Vec<String> = LNK_LINK_FIELDS
                        .iter()
                        .map(|f| Self::field_str(&instance, f))
                        .collect();
                    // SELM resolution with OFFS/SHFT bias (fanoutRecord.c).
                    let sel = select_link_indices_ex(
                        SelmKind::FanoutSeq,
                        selm,
                        seln,
                        offs,
                        shft,
                        links.len(),
                    );
                    Some((sel, MultiOut::Fanout(links), None))
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
                    let val = match phase {
                        MultiOutPhase::Output { skip_out: true } => None,
                        MultiOutPhase::Output { skip_out: false } => instance.record.val(),
                        // Unreachable: the phase gate above already returned
                        // for a dfanout reached in the forward-link tail.
                        MultiOutPhase::ForwardLink => return MultiOutDispatch::default(),
                    };
                    let links: Vec<String> = DFANOUT_LINK_FIELDS
                        .iter()
                        .map(|f| Self::field_str(&instance, f))
                        .collect();
                    // dfanout Specified is 1-based; Mask has no SHFT
                    // (dfanoutRecord.c:308-339).
                    let sel =
                        select_link_indices_ex(SelmKind::Dfanout, selm, seln, 0, 0, links.len());
                    Some((sel, MultiOut::Dfanout(links), val))
                }
                "seq" => {
                    let selm = Self::field_i16(&instance, "SELM");
                    let seln = Self::field_u16(&instance, "SELN");
                    let offs = Self::field_i16(&instance, "OFFS");
                    let shft = Self::field_i16(&instance, "SHFT");
                    // C parity (seqRecord.c:86): 16 link groups 0..F,
                    // each DOLn / DOn (value storage) / DLYn / LNKn.
                    let dol_names = [
                        "DOL0", "DOL1", "DOL2", "DOL3", "DOL4", "DOL5", "DOL6", "DOL7", "DOL8",
                        "DOL9", "DOLA", "DOLB", "DOLC", "DOLD", "DOLE", "DOLF",
                    ];
                    let lnk_names = LNK_LINK_FIELDS;
                    let dly_names = [
                        "DLY0", "DLY1", "DLY2", "DLY3", "DLY4", "DLY5", "DLY6", "DLY7", "DLY8",
                        "DLY9", "DLYA", "DLYB", "DLYC", "DLYD", "DLYE", "DLYF",
                    ];
                    let do_names = [
                        "DO0", "DO1", "DO2", "DO3", "DO4", "DO5", "DO6", "DO7", "DO8", "DO9",
                        "DOA", "DOB", "DOC", "DOD", "DOE", "DOF",
                    ];
                    let groups: Vec<SeqGroup> = (0..16)
                        .map(|i| SeqGroup {
                            dol: Self::field_str(&instance, dol_names[i]),
                            lnk: Self::field_str(&instance, lnk_names[i]),
                            dly: instance
                                .record
                                .get_field(dly_names[i])
                                .and_then(|v| v.to_f64())
                                .unwrap_or(0.0),
                            dov: instance
                                .record
                                .get_field(do_names[i])
                                .and_then(|v| v.to_f64())
                                .unwrap_or(0.0),
                        })
                        .collect();
                    let sel = select_link_indices_ex(
                        SelmKind::FanoutSeq,
                        selm,
                        seln,
                        offs,
                        shft,
                        groups.len(),
                    );
                    Some((sel, MultiOut::Seq(groups), None))
                }
                _ => None,
            }
        };

        let (sel, payload, val) = match dispatch_info {
            Some(info) => info,
            None => return MultiOutDispatch::default(),
        };
        debug_assert!(sel.indices.iter().all(|&i| i < payload.len()));
        // Single-owner invariant: every record type that produces a
        // `MultiOut` payload here MUST be listed in
        // `multi_output_dispatch_owned` so the generic
        // `multi_output_links` block in `processing.rs` skips it. If
        // this fires, the two lists have diverged and the skipped
        // type would be dispatched twice per cycle.
        debug_assert!(multi_output_dispatch_owned(rec.read().record.record_type()));

        // C raises SOFT_ALARM/INVALID_ALARM when SELN/OFFS/SHFT resolve
        // out of range (fanoutRecord.c:116, dfanoutRecord.c:317,
        // seqRecord.c:157). The value-put phase (dfanout/seq) runs PRE-commit,
        // so its out-of-range alarm must fold into the pending nsev the caller
        // commits THIS cycle — captured here and returned below, not
        // raised+posted now (a direct SEVR raise would be clobbered by the
        // caller's `recGblResetAlarms`). A fanout dispatches in the
        // post-commit tail and raises it directly into the committed SEVR,
        // posting SEVR/STAT immediately (apply_selm_alarm).
        let pending_selm_alarm = sel.alarm;
        if !is_value_phase {
            Self::apply_selm_alarm(rec, sel.alarm);
        }
        let indices = sel.indices;
        // C `dfanoutRecord.c` push_values raises LINK_ALARM/MAJOR per failed
        // dbPutLink; accumulated across the selected OUT links below.
        let mut link_failed = false;

        match payload {
            MultiOut::Fanout(links) => {
                for idx in indices {
                    let link_str = &links[idx];
                    if link_str.is_empty() {
                        continue;
                    }
                    // fanout `LNK1`..`LNKF` are `DBF_FWDLINK`
                    // (`fanoutRecord.dbd.pod:144`), so C masks their modifiers
                    // to `pvlOptCA` alone (`dbStaticLib.c:2390`).
                    let parsed = crate::server::record::parse_forward_link_v2(link_str);
                    if let crate::server::record::ParsedLink::Db(ref db) = parsed {
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
                        self.process_target(
                            &db.target().record,
                            ProcessTargetGate::ScanPassive,
                            src_putf,
                            src_notify.as_ref(),
                            visited,
                            depth,
                        );
                    }
                }
            }
            MultiOut::Dfanout(links) => {
                if let Some(ref val) = val {
                    for idx in indices {
                        let link_str = &links[idx];
                        if link_str.is_empty() {
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
                        // `write_db_link_value` (write-path target
                        // processing), so no per-call downgrade is needed.
                        let parsed = crate::server::record::parse_output_link_v2(link_str);
                        // Through the put owner, so a failed OUTn raises the
                        // record's LINK_ALARM/INVALID from inside the put (C
                        // `dbPutLink`'s `setLinkAlarm`) — an external
                        // `ca://`/`pva://` OUTn goes through the link set's
                        // `putValue` exactly as a DB link does
                        // (dbLink.c:434-448). `link_failed` additionally
                        // carries dfanout's OWN LINK_ALARM/MAJOR
                        // (`dfanoutRecord.c:311-312`), which C raises on top of
                        // (and is subsumed by) the put's INVALID.
                        if self.write_out_link_value(
                            rec,
                            &parsed,
                            val.clone(),
                            OutLinkSrc {
                                field: DFANOUT_LINK_FIELDS[idx],
                                ..out_src
                            },
                            visited,
                            depth,
                        ) {
                            link_failed = true;
                        }
                    }
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
                let active: Vec<usize> = indices
                    .into_iter()
                    .filter(|&i| {
                        Self::seq_link_is_real(&groups[i].lnk)
                            || Self::seq_link_is_real(&groups[i].dol)
                    })
                    .collect();
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
                    let rec_name = rec.read().name.clone();
                    let prio = {
                        let instance = rec.write();
                        instance.enter_pact();
                        instance.common.callback_priority()
                    };
                    let db = self.clone();
                    let rec = rec.clone();
                    crate::runtime::task::spawn_background(prio, async move {
                        for idx in active {
                            let dly = groups[idx].dly;
                            if dly > 0.0 {
                                crate::runtime::task::sleep_background(
                                    crate::runtime::time::duration_from_secs(dly),
                                )
                                .await;
                            }
                            {
                                // C `processCallback`'s `dbScanLock` /
                                // `dbScanUnlock` pair (`seqRecord.c:252`,
                                // `:274`) — held for this group alone.
                                let _gate = db.lock_record(&rec_name);
                                let mut visited = HashSet::new();
                                db.seq_group_step(&rec, &rec_name, &groups, idx, &mut visited, 0);
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

        // Value-put phase (dfanout / seq): return the pending alarm the C
        // record raises alongside its puts, for the caller to fold into `nsev`
        // before `recGblResetAlarms`. C raises at most one: SOFT_ALARM/INVALID
        // for a `seln` out of range (dfanoutRecord.c:317, seqRecord.c:157 — no
        // push at all) OR dfanout's own LINK_ALARM/MAJOR for a failed
        // dbPutLink (dfanoutRecord.c:312/324/333). They are mutually exclusive
        // in C's control flow; if both were somehow set, fold the higher
        // severity (recGblSetSevr is raise-only). `link_failed` is dfanout-only
        // — seq's failed LNKn put raises LINK_ALARM/INVALID from inside the put
        // owner (`write_out_link_value`), into the same pending alarm, and
        // seqRecord.c adds nothing on top of it.
        if is_value_phase {
            let link_alarm = if link_failed {
                Some((
                    crate::server::recgbl::alarm_status::LINK_ALARM,
                    AlarmSeverity::Major,
                ))
            } else {
                None
            };
            return MultiOutDispatch {
                alarm: match (pending_selm_alarm, link_alarm) {
                    (Some(a), Some(b)) => Some(if (a.1 as u16) >= (b.1 as u16) { a } else { b }),
                    (a, b) => a.or(b),
                },
                went_async: false,
            };
        }
        MultiOutDispatch::default()
    }

    /// C `dbLinkIsConstant` negated — a real link is a DB/CA/PVA PV
    /// reference, not an empty or numeric-literal field.
    fn seq_link_is_real(s: &str) -> bool {
        !s.is_empty()
            && !matches!(
                crate::server::record::parse_link_v2(s),
                crate::server::record::ParsedLink::Constant(_)
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
    fn seq_group_step(
        &self,
        rec: &Arc<parking_lot::RwLock<RecordInstance>>,
        rec_name: &str,
        groups: &[SeqGroup],
        idx: usize,
        visited: &mut HashSet<String>,
        depth: usize,
    ) {
        // DOn value-storage field names (`linkGrp.dov`), index-aligned with
        // the LNKn/DOLn groups.
        const DO_NAMES: [&str; 16] = [
            "DO0", "DO1", "DO2", "DO3", "DO4", "DO5", "DO6", "DO7", "DO8", "DO9", "DOA", "DOB",
            "DOC", "DOD", "DOE", "DOF",
        ];
        let grp = &groups[idx];
        let lnk_real = Self::seq_link_is_real(&grp.lnk);
        let dol_real = Self::seq_link_is_real(&grp.dol);
        // C `seqRecord.c:259` `dbGetLink(&dol, DBR_DOUBLE, &dov)`: read DOLn
        // into the DOn value field. A constant/empty DOL — or a failed read —
        // leaves DOn at its previous value. Through the read owner, so a dead
        // DOLn raises `setLinkAlarm` (LINK/INVALID, AMSG `field DOL0`) on the
        // seq itself the way every other `dbGetLink` in the port does.
        let new_dov = if dol_real {
            let dol_parsed = crate::server::record::parse_link_v2(&grp.dol);
            self.db_get_input_link(rec, DOL_LINK_FIELDS[idx], &dol_parsed, visited, depth)
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
        self.rec_gbl_get_time_stamp(rec);
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
            let (src_putf, src_notify, src_alarm) = {
                let guard = rec.read();
                (
                    guard.common.putf,
                    guard.notify.clone(),
                    LinkAlarm::pending(&guard.common),
                )
            };
            let lnk_parsed = crate::server::record::parse_output_link_v2(&grp.lnk);
            // Through the put owner: a failed LNKn `dbPutLink` raises the seq
            // record's LINK_ALARM/INVALID from inside the put (C
            // `dbLink.c:444-446`).
            self.write_out_link_value(
                rec,
                &lnk_parsed,
                EpicsValue::Double(new_dov),
                OutLinkSrc {
                    putf: src_putf,
                    notify: src_notify.as_ref(),
                    alarm: &src_alarm,
                    field: LNK_LINK_FIELDS[idx],
                },
                visited,
                depth,
            );
        }
        // C `seqRecord.c:266-268`: store DOn and post a DBE_VALUE|DBE_LOG
        // monitor only when it changed. The field already holds the old value
        // (snapshot read), so `post_fields` (write + post) is needed only on
        // change.
        if new_dov != grp.dov {
            let _ = self.post_fields(
                rec_name,
                vec![(DO_NAMES[idx].to_string(), EpicsValue::Double(new_dov))],
            );
        }
    }

    /// Post the software event named by an `event` record's `VAL`.
    ///
    /// Mirrors C `eventRecord.c:120` `postEvent(prec->epvt)` — every
    /// `process()` of an event record posts its event, waking the
    /// `SCAN="Event"` records whose `EVNT` resolves to that name.
    /// No-op for any other record type, or when `VAL` is empty /
    /// resolves to event 0 (`eventNameToHandle` returns NULL).
    pub(crate) fn dispatch_event_record(&self, rec: &Arc<parking_lot::RwLock<RecordInstance>>) {
        let event_name = {
            let instance = rec.read();
            if instance.record.record_type() != "event" {
                return;
            }
            match instance.record.get_field("VAL") {
                Some(EpicsValue::String(s)) => s.as_str_lossy().into_owned(),
                _ => return,
            }
        };
        if event_name.trim().is_empty() {
            return;
        }
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
                db.post_event_named(&event_name).await;
            },
        );
    }

    /// Register a CP link: when source_record changes, process target_record.
    ///
    /// Both names are normalised to canonical form so the cp_links
    /// map's key/value always match the canonical record name that
    /// `dispatch_cp_targets` uses for lookup. Without this, a user
    /// who wrote `INP="ALIAS_NAME CP"` in their .db file would
    /// register the CP edge under the alias key and then never see
    /// the target processed (the source record's canonical-name
    /// dispatch would miss).
    /// `passive_only` is `true` for a CPP edge (process the target only when
    /// its `SCAN` is Passive) and `false` for CP (always process). When the
    /// same source→target edge is registered from both a CP and a CPP link,
    /// CP dominates: the merged edge keeps `passive_only == false`, matching
    /// C, where an unconditional CP `CA_DBPROCESS` overrides any CPP gate on
    /// the same record.
    pub async fn register_cp_link(
        &self,
        source_record: &str,
        target_record: &str,
        passive_only: bool,
    ) {
        let source = self
            .resolve_alias(source_record)
            .unwrap_or_else(|| source_record.to_string());
        let target = self
            .resolve_alias(target_record)
            .unwrap_or_else(|| target_record.to_string());
        self.inner.cp_links.update(|cp| {
            let targets = cp.entry(source).or_default();
            if let Some(existing) = targets.iter_mut().find(|t| t.record == target) {
                existing.passive_only = existing.passive_only && passive_only;
            } else {
                targets.push(super::CpTarget {
                    record: target,
                    passive_only,
                });
            }
        });
    }

    /// Get target edges to process when `source_record` changes (CP/CPP links).
    pub fn get_cp_targets(&self, source_record: &str) -> Vec<super::CpTarget> {
        self.inner
            .cp_links
            .load()
            .get(source_record)
            .cloned()
            .unwrap_or_default()
    }

    /// Register an EXTERNAL CP/CPP link: when the remote PV `external_pv`
    /// (a cross-IOC CA/PVA source, e.g. `OTHER:PV` from
    /// `INP="OTHER:PV CP"`) changes, process `target_record`.
    ///
    /// Twin of [`Self::register_cp_link`] for cross-IOC sources. The key is
    /// the **scheme-stripped external PV name** — it is not a local record,
    /// so it is NOT alias-resolved. It MUST equal the name the calink/pvalink
    /// monitor dispatches under: that monitor is opened through the lset,
    /// which strips the `ca://` / `pva://` scheme first, so its `pv_name`
    /// (passed to [`Self::dispatch_external_cp_targets`]) is the bare PV.
    /// Stripping the same schemes here — the set `Self::resolve_external_pv`
    /// already knows — guarantees the registry key and the dispatch key can
    /// never diverge. The `target_record` (the local holder) IS alias-resolved
    /// so the dispatch processes the canonical record. CP dominates CPP on a
    /// merged edge, identical to the local path.
    pub async fn register_external_cp_link(
        &self,
        external_pv: &str,
        target_record: &str,
        passive_only: bool,
    ) {
        let key = external_pv
            .strip_prefix("ca://")
            .or_else(|| external_pv.strip_prefix("pva://"))
            .unwrap_or(external_pv);
        let target = self
            .resolve_alias(target_record)
            .unwrap_or_else(|| target_record.to_string());
        self.inner.external_cp_links.update(|cp| {
            let targets = cp.entry(key.to_string()).or_default();
            if let Some(existing) = targets.iter_mut().find(|t| t.record == target) {
                existing.passive_only = existing.passive_only && passive_only;
            } else {
                targets.push(super::CpTarget {
                    record: target,
                    passive_only,
                });
            }
        });
    }

    /// Get holder edges to process when the remote PV `external_pv` changes
    /// (external CA/PVA CP/CPP links).
    pub fn get_external_cp_targets(&self, external_pv: &str) -> Vec<super::CpTarget> {
        self.inner
            .external_cp_links
            .load()
            .get(external_pv)
            .cloned()
            .unwrap_or_default()
    }

    /// Every external PV name that has at least one CP/CPP holder edge.
    /// The calink/pvalink integration warms (opens a monitor for) each of
    /// these at iocInit so the remote source has a live subscription whose
    /// callback can drive [`Self::dispatch_external_cp_targets`] — a Passive
    /// CP holder is otherwise never read and its monitor never opens.
    pub async fn external_cp_pv_names(&self) -> Vec<String> {
        self.inner
            .external_cp_links
            .load()
            .keys()
            .cloned()
            .collect()
    }

    /// Every DISTINCT external PV name this database's link fields name, in
    /// any direction and under any policy — the set the link registry
    /// converges to once iocInit's staged opens have all landed.
    ///
    /// Distinct from [`Self::external_cp_pv_names`], which is the CP/CPP
    /// subset, and from the counts [`Self::setup_cp_links`] and
    /// [`Self::setup_external_link_opens`] print: those count *link fields*
    /// (two records pointing at one upstream PV are two links), whereas a
    /// link set holds one entry per PV. Reporting a registry count against a
    /// field count would never agree.
    ///
    /// The enumeration goes through [`super::PvDatabase::record_link_fields`]
    /// and `external_pv_name`, the same two owners both iocInit link phases
    /// use, so "what is an external link" cannot diverge between staging a
    /// link and counting it.
    ///
    /// Sorted, so a caller can print it as a stable operator-facing list.
    pub async fn external_link_pv_names(&self) -> Vec<String> {
        let mut seen: Vec<String> = Vec::new();
        for record_name in &self.all_record_names().await {
            for (_field, _raw, parsed) in self.record_link_fields(record_name) {
                let Some(pv) = parsed.external_pv_name() else {
                    continue;
                };
                if !seen.iter().any(|s| s == pv.as_ref()) {
                    seen.push(pv.into_owned());
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
    /// **Deviation, deliberate.** Because C skips `dbDbInitLink` for a
    /// `CA`/`CP`/`CPP` link, C makes a CP link to a LOCAL record a CA link
    /// too. This function does not: it leaves a local CP target as `Db`,
    /// because the `ca` link set lives in `epics-ca-rs` and an IOC built on
    /// `epics-base-rs` alone would be left with a local CP link that resolves
    /// nowhere. What C buys with that conversion — a holder processed only on
    /// a `DBE_VALUE|DBE_ALARM` post, never on a bare source process — is paid
    /// for instead at the trigger, by the `CyclePosts` gate in
    /// `PvDatabase::dispatch_cp_targets`.
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
        if let Some(scheme) = parsed.required_link_set_scheme()
            && self.inner.link_sets.load().get(scheme).is_none()
        {
            return crate::server::record::ParsedLink::None;
        }
        let crate::server::record::ParsedLink::Db(db) = &parsed else {
            return parsed;
        };
        if self.has_name_no_resolve(&db.target().record) {
            return parsed;
        }
        crate::server::record::ParsedLink::Ca(crate::server::record::CaLink {
            // `DbLink::channel_name` is the same string C keeps in
            // `pv_link.pvname` across the `dbChannelCreate` → `dbCaAddLink`
            // fallthrough, so the DB target and the CA channel cannot drift.
            pv: db.pvname(),
            monitor_switch: db.monitor_switch,
            policy: db.policy,
        })
    }

    /// Commit C `dbInitLink`'s locality decision into every record's parsed
    /// link cache — the iocInit pass that makes the rule STATE rather than a
    /// test each read path repeats.
    ///
    /// Runs after all records have loaded and before `setup_cp_links`, which
    /// is C's ordering: `initPVLinks` walks the loaded database once. Only the
    /// `COMMON_LINK_FIELDS`
    /// have a parse cache to commit into; `INPA..`/`DOL..` and the `lnkCalc`
    /// arguments are re-parsed from their raw text each process cycle and keep
    /// the read path's runtime locality fallback
    /// (`read_target_value`) instead.
    ///
    /// The decision is taken from the field's RAW text, not from the cache:
    /// the cache is only guaranteed fresh when the field was written through
    /// `put_common_field`, and a link whose raw string was installed some
    /// other way must still be initialised. Only a genuine `Db` → `Ca`
    /// conversion is written back, so a cache that legitimately differs from
    /// its raw text is left alone.
    ///
    /// Returns the number of links converted.
    pub async fn initialize_link_locality(&self) -> usize {
        use crate::server::record::record_instance::COMMON_LINK_FIELDS;
        let names = self.all_record_names().await;
        let mut converted = 0usize;
        let mut refused = 0usize;
        let installed_schemes = {
            let mut s = self.inner.link_sets.load().schemes();
            s.sort();
            s
        };
        for name in &names {
            let Some(rec) = self.get_record(name) else {
                continue;
            };
            // Decide before taking the record's write lock: the locality query
            // reads the database's record map, and nothing else in this crate
            // holds a record-instance lock across that map.
            let raw: Vec<(&str, String)> = {
                let inst = rec.read();
                COMMON_LINK_FIELDS
                    .iter()
                    .filter_map(|(field, _)| {
                        inst.common_link_text(field)
                            .filter(|t| !t.is_empty())
                            .map(|t| (*field, t.to_string()))
                    })
                    .collect()
            };
            let mut rewrites: Vec<(&str, crate::server::record::ParsedLink)> = Vec::new();
            for (field, text) in &raw {
                let ftype = COMMON_LINK_FIELDS
                    .iter()
                    .find(|(f, _)| f == field)
                    .map(|(_, t)| *t)
                    .expect("field came from COMMON_LINK_FIELDS");
                let parsed = crate::server::record::parse_link_field(text, ftype);
                let unserviceable = parsed.required_link_set_scheme();
                if unserviceable.is_none()
                    && !matches!(parsed, crate::server::record::ParsedLink::Db(_))
                {
                    continue;
                }
                let next = self.db_init_link_locality(parsed);
                match (&next, unserviceable) {
                    (crate::server::record::ParsedLink::Ca(_), _) => rewrites.push((field, next)),
                    // The refusal `db_init_link_locality` just made, said out
                    // loud. This is the only pass that runs once per IOC, so
                    // it is the only place the operator can be told without
                    // the message repeating on every `record_link_fields`.
                    (crate::server::record::ParsedLink::None, Some(scheme)) => {
                        eprintln!(
                            "iocInit: {name}.{field} names link type '{scheme}' but no \
                             '{scheme}' link set is installed in this IOC (installed: \
                             {installed_schemes:?}); the link is refused: {text}"
                        );
                        refused += 1;
                        rewrites.push((field, next));
                    }
                    _ => {}
                }
            }
            if rewrites.is_empty() {
                continue;
            }
            let mut inst = rec.write();
            for (field, link) in rewrites {
                let is_refusal = matches!(link, crate::server::record::ParsedLink::None);
                if let Some(slot) = inst.common_link_cache_mut(field) {
                    *slot = link;
                    if !is_refusal {
                        converted += 1;
                    }
                }
            }
        }
        if converted > 0 {
            eprintln!("iocInit: {converted} non-local DB link(s) made external");
        }
        if refused > 0 {
            eprintln!(
                "iocInit: {refused} link(s) refused — no link set is installed for the \
                 scheme they name"
            );
        }
        converted
    }

    /// Classify one parsed CP/CPP link into the local or the external CP
    /// trigger registry.
    ///
    /// A link with no CP/CPP policy (`cp_passive_only() == None`), and
    /// every non-`Db`/`Ca` variant, is ignored.
    ///
    /// `Ca`: an external `ca://OTHER CP` holder always drives the cross-IOC
    /// path — C `dbCa.c` `eventCallback` adds `CA_DBPROCESS` for every CP link
    /// (`dbCa.c:958-962`). Note `OTHER CP CA` is NOT such a link: `CA` and `CP`
    /// are mutually exclusive process classes and `CA` is matched first
    /// (`dbStaticLib.c:2369-2373`), so it is a plain CA link with no CP policy.
    ///
    /// `Db`: a *local* target only. The locality decision is not made here —
    /// [`super::PvDatabase::record_link_fields`] has already mapped every
    /// enumerated link through [`Self::db_init_link_locality`], so a CP/CPP
    /// link to a non-local target arrives at the `Ca` arm above and lands in
    /// `ext_links` by the same route an explicit `ca://OTHER CP` does. That
    /// rule used to live in this method, which is why it applied to CP/CPP
    /// links only.
    fn classify_cp_link(
        &self,
        parsed: crate::server::record::ParsedLink,
        target_name: &str,
        db_links: &mut Vec<(String, String, bool)>,
        ext_links: &mut Vec<(String, String, bool)>,
    ) {
        match parsed {
            crate::server::record::ParsedLink::Db(db) => {
                if let Some(passive_only) = db.policy.cp_passive_only() {
                    db_links.push((db.target().record, target_name.to_string(), passive_only));
                }
            }
            crate::server::record::ParsedLink::Ca(ca) => {
                if let Some(passive_only) = ca.policy.cp_passive_only() {
                    ext_links.push((ca.pv, target_name.to_string(), passive_only));
                }
            }
            _ => {}
        }
    }

    /// Scan all records for CP/CPP input links and register them. Local
    /// `Db` links land in the local CP registry; external `Ca` links land
    /// in the external CP registry, whose holders are processed by the
    /// calink monitor callback.
    pub async fn setup_cp_links(&self) {
        let names = self.all_record_names().await;
        let mut db_links: Vec<(String, String, bool)> = Vec::new();
        let mut ext_links: Vec<(String, String, bool)> = Vec::new();

        for target_name in &names {
            // Enumerate this record's link-bearing fields through the
            // single shared owner (`record_link_fields`) so the CA CP/CPP
            // setup here and the pvalink install scan can never diverge on
            // which fields count as links. That owner already applies C
            // `dbInitLink`'s locality fallthrough
            // (`Self::db_init_link_locality`), so a CP/CPP link to a
            // non-local target arrives here as `Ca` and needs no
            // CP-specific conversion; the holder's own read path is rewired
            // by `initialize_link_locality`, which commits the same decision
            // into the parse cache for every link, CP or not.
            for (_field, _raw, parsed) in self.record_link_fields(target_name) {
                self.classify_cp_link(parsed, target_name, &mut db_links, &mut ext_links);
            }
        }

        let db_count = db_links.len();
        for (source, target, passive_only) in db_links {
            self.register_cp_link(&source, &target, passive_only).await;
        }
        let ext_count = ext_links.len();
        for (external_pv, target, passive_only) in ext_links {
            self.register_external_cp_link(&external_pv, &target, passive_only)
                .await;
        }
        if db_count > 0 {
            eprintln!("iocInit: {db_count} CP link subscriptions");
        }
        // Warm external CP/CPP links. A Passive holder of an external CP
        // link is never scanned, so its link never opens lazily and the
        // calink monitor that drives `dispatch_external_cp_targets` is never
        // created (chicken-and-egg). Open each external CP PV now so its
        // monitor is live at iocInit — the C `dbCa.c` "add the link at init"
        // analogue (`dbCaAddLink`). `resolve_external_pv` routes through the
        // registered lset's lazy-open path (the same path a record read
        // uses); a no-op when no matching lset is installed (calink off).
        if ext_count > 0 {
            let ext_pvs = self.external_cp_pv_names().await;
            for pv in &ext_pvs {
                let _ = self.resolve_external_pv(pv);
            }
            eprintln!(
                "iocInit: {ext_count} external CP link subscriptions ({} PVs warmed)",
                ext_pvs.len()
            );
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
    /// `PvDatabase::stage_external_link_open_by_name` →
    /// `stage_external_link_open`, so the connect still runs on the link work
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
        let names = self.all_record_names().await;
        let mut staged = 0usize;
        for record_name in &names {
            for (_field, _raw, parsed) in self.record_link_fields(record_name) {
                // `external_pv_name` is `Some` exactly for the external
                // variants (`Ca` / `Pva` / `PvaJson`) and carries the key the
                // link set is addressed with. A local `Db` link, a
                // `Constant`, a hardware link and a `lnkCalc` link all report
                // `None` — C's `dbDbInitLink` / `dbConstInitLink` /
                // `dbJLinkInit` paths, none of which reach `dbCaAddLink`.
                let Some(pv) = parsed.external_pv_name() else {
                    continue;
                };
                if self.stage_external_link_open_by_name(&pv) {
                    staged += 1;
                }
            }
        }
        if staged > 0 {
            eprintln!("iocInit: {staged} external link opens staged");
        }
        staged
    }
}

#[cfg(test)]
mod out_link_put_fail_tests {
    use super::{LinkAlarm, OutLinkSrc};
    use crate::server::database::PvDatabase;
    use crate::server::record::{AlarmSeverity, DbLink, LinkProcessPolicy, MonitorSwitch};
    use crate::server::records::calc::CalcRecord;
    use crate::types::EpicsValue;
    use std::collections::HashSet;

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
        let alarm = LinkAlarm {
            stat: 0,
            sevr: AlarmSeverity::NoAlarm,
            amsg: String::new(),
        };
        let src = OutLinkSrc {
            putf: false,
            notify: None,
            alarm: &alarm,
            field: "OUT",
        };
        let mut visited = HashSet::new();

        db.write_db_link_value(
            &link,
            EpicsValue::String("NOT_A_MENU_CHOICE".into()),
            src,
            &mut visited,
            0,
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
        let alarm = LinkAlarm {
            stat: 0,
            sevr: AlarmSeverity::NoAlarm,
            amsg: String::new(),
        };
        let src = OutLinkSrc {
            putf: false,
            notify: None,
            alarm: &alarm,
            field: "OUT",
        };
        let mut visited = HashSet::new();

        db.write_db_link_value(&link, EpicsValue::DoubleArray(vec![]), src, &mut visited, 0);

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
    use super::OutLinkSrc;
    use crate::server::database::{LinkPutOp, LinkSet, PvDatabase};
    use crate::server::record::{AlarmSeverity, DbLink, LinkProcessPolicy, MonitorSwitch};
    use crate::server::records::calc::CalcRecord;
    use crate::types::EpicsValue;
    use std::collections::HashSet;
    use std::sync::{Arc, Mutex};

    /// A link set that records every `put_value` it receives so a test
    /// can assert a non-local OUT-link write reached the external (CA)
    /// put path instead of being dropped by a local `dbPut`.
    struct RecordingLset {
        puts: Arc<Mutex<Vec<(String, EpicsValue)>>>,
    }
    #[async_trait::async_trait]
    impl LinkSet for RecordingLset {
        fn is_connected(&self, _: &str) -> bool {
            true
        }
        fn get_cached_value(&self, _: &str) -> Option<EpicsValue> {
            None
        }
        async fn get_value(&self, name: &str) -> Option<EpicsValue> {
            self.get_cached_value(name)
        }
        async fn put_value(
            &self,
            name: &str,
            value: EpicsValue,
            _op: LinkPutOp,
        ) -> Result<(), String> {
            self.puts.lock().unwrap().push((name.to_string(), value));
            Ok(())
        }
    }

    fn out_src(alarm: &super::LinkAlarm) -> OutLinkSrc<'_> {
        OutLinkSrc {
            putf: false,
            notify: None,
            alarm,
            field: "OUT",
        }
    }

    fn no_alarm() -> super::LinkAlarm {
        super::LinkAlarm {
            stat: 0,
            sevr: AlarmSeverity::NoAlarm,
            amsg: String::new(),
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
        let puts = Arc::new(Mutex::new(Vec::new()));
        db.register_link_set("ca", Arc::new(RecordingLset { puts: puts.clone() }))
            .await;

        // "OTHER:PV" is never added as a local record -> non-local.
        let link = DbLink::new(
            "OTHER:PV",
            LinkProcessPolicy::NoProcess,
            MonitorSwitch::NoMaximize,
        );
        let alarm = no_alarm();
        let mut visited = HashSet::new();
        db.write_db_link_value(
            &link,
            EpicsValue::Double(42.0),
            out_src(&alarm),
            &mut visited,
            0,
        );
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
        let alarm = no_alarm();
        let mut visited = HashSet::new();
        db.write_db_link_value(
            &link,
            EpicsValue::Double(7.0),
            out_src(&alarm),
            &mut visited,
            0,
        );

        assert!(
            puts.lock().unwrap().is_empty(),
            "a local OUT-link write must not reach the external put path"
        );
        assert!(
            matches!(db.get_pv("TGT.VAL").unwrap(), EpicsValue::Double(v) if v == 7.0),
            "the local target must hold the written value"
        );
    }

    /// A link set that records every `scan_forward` it receives so a test
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
        fn is_connected(&self, _: &str) -> bool {
            self.connected
        }
        fn get_cached_value(&self, _: &str) -> Option<EpicsValue> {
            None
        }
        async fn get_value(&self, name: &str) -> Option<EpicsValue> {
            self.get_cached_value(name)
        }
        fn scan_forward(&self, name: &str) -> Result<(), String> {
            self.forwards.lock().unwrap().push(name.to_string());
            if self.connected {
                Ok(())
            } else {
                Err("Disconn".into())
            }
        }
    }

    /// `scan_forward_external_pv` must resolve the scheme and delegate to
    /// the registered lset's `scan_forward` — the FWD-link twin of
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

        db.scan_forward_external_pv("pva://OTHER:PROC")
            .expect("a connected forward must succeed");

        let captured = forwards.lock().unwrap();
        assert_eq!(captured.len(), 1);
        assert_eq!(captured[0], "OTHER:PROC");
    }

    /// End-to-end: a record whose FLNK targets an external `pva://` PV
    /// must fire the link set's `scan_forward` when it processes — the
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

        let mut visited = HashSet::new();
        db.process_record_with_links("SRC", &mut visited, 0)
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

        let mut visited = HashSet::new();
        db.process_record_with_links("SRC", &mut visited, 0)
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
}

#[cfg(test)]
mod cp_link_locality_tests {
    use crate::server::database::PvDatabase;
    use crate::server::record::ParsedLink;
    use crate::server::records::ai::AiRecord;

    /// A CP/CPP link with no explicit `CA` modifier whose target is NOT a
    /// local record must be served as an external (CA) link — both the
    /// trigger and the value read. C `dbInitLink` (`dbLink.c:118-122`) tests
    /// the `CA`/`CP`/`CPP` modifier FIRST and skips `dbDbInitLink` entirely
    /// when one is present, so in C a CP link is a CA link whether or not the
    /// named record is local; `dbLink.c:128` computes `isLocal` only to pick
    /// the init-callback hint. The port keeps a LOCAL CP target as a `Db`
    /// link instead (see `cp_link_to_local_target_stays_db`) and restores the
    /// C semantics at the trigger, so only the non-local case converts here.
    ///
    /// Here `INP="OTHER:PV CP"` parses to `ParsedLink::Db` (bare name, no
    /// `CA`), but `OTHER:PV` is not local. The holder's `parsed_inp` must be
    /// rewritten to `ParsedLink::Ca` (so the read routes through
    /// `resolve_external_pv`) and `OTHER:PV` must be registered as an
    /// external CP trigger.
    ///
    /// The two halves now have two owners, matching C's two calls:
    /// `initialize_link_locality` is `dbInitLink`'s conversion and applies to
    /// EVERY link, CP or not; `setup_cp_links` only registers the trigger.
    /// The rewrite used to live inside `setup_cp_links`, which is why it
    /// reached CP/CPP links alone.
    #[epics_macros_rs::epics_test]
    async fn cp_link_to_nonlocal_target_forced_external() {
        let db = PvDatabase::new();
        db.add_record("HOLDER", Box::new(AiRecord::new(0.0)))
            .await
            .unwrap();
        {
            let rec = db.get_record("HOLDER").unwrap();
            rec.write().common.inp = "OTHER:PV CP".to_string();
        }

        db.initialize_link_locality().await;
        db.setup_cp_links().await;

        let inp = db.get_record("HOLDER").unwrap().read().parsed_inp.clone();
        match inp {
            ParsedLink::Ca(ca) => assert_eq!(
                ca.pv, "OTHER:PV",
                "non-local CP link must carry the verbatim PV name"
            ),
            other => panic!("non-local CP link must be forced to Ca, got {other:?}"),
        }
        assert!(
            db.external_cp_pv_names()
                .await
                .contains(&"OTHER:PV".to_string()),
            "non-local CP link must be registered as an external CP trigger"
        );
    }

    /// A CP link whose target IS a local record keeps the `Db` SHAPE — and
    /// that is a deliberate port DEVIATION, not C parity.
    ///
    /// C `dbInitLink` (`dbLink.c:118-122`) would make this link a CA link:
    /// the modifier test precedes and short-circuits `dbDbInitLink`, so `SRC`
    /// being local changes nothing about the link's class. The port cannot
    /// follow that literally — the `ca` link set lives in `epics-ca-rs`, which
    /// `epics-base-rs` does not depend on, so converting a local CP link to
    /// `Ca` would leave it unresolvable (and its holder never processed) in a
    /// bare `epics-base-rs` IOC.
    ///
    /// What this test therefore pins is the deviation TOGETHER with what
    /// compensates for it: the link keeps the `Db` shape, it is NOT an
    /// external CP trigger, and it IS registered in the LOCAL CP registry —
    /// the registry `PvDatabase::dispatch_cp_targets` drives, and which since
    /// the `CyclePosts` gate fires only on a `DBE_VALUE|DBE_ALARM` post from
    /// `SRC`, exactly as C's CA subscription would (`dbCa.c:1225-1229` →
    /// `cadef.h:2010-2011`). The observable rule is proven by
    /// `tests/cp_link_trigger_is_a_post.rs`; a `Db` shape with no local
    /// registration would satisfy the old assertion and still never process
    /// the holder.
    #[epics_macros_rs::epics_test]
    async fn cp_link_to_local_target_stays_db() {
        let db = PvDatabase::new();
        db.add_record("SRC", Box::new(AiRecord::new(1.0)))
            .await
            .unwrap();
        db.add_record("HOLDER", Box::new(AiRecord::new(0.0)))
            .await
            .unwrap();
        {
            let rec = db.get_record("HOLDER").unwrap();
            let mut inst = rec.write();
            inst.common.inp = "SRC CP".to_string();
            // Seed the parse cache as load would, so the assertion proves
            // setup_cp_links leaves a local CP link untouched.
            inst.parsed_inp = crate::server::record::parse_link_v2("SRC CP");
        }

        db.initialize_link_locality().await;
        db.setup_cp_links().await;

        let inp = db.get_record("HOLDER").unwrap().read().parsed_inp.clone();
        assert!(
            matches!(inp, ParsedLink::Db(_)),
            "the port keeps a local CP link in the Db shape (C would make it a \
             CA link); got {inp:?}"
        );
        assert!(
            !db.external_cp_pv_names().await.contains(&"SRC".to_string()),
            "a local CP target must not be registered as an external CP link"
        );
        assert!(
            db.get_cp_targets("SRC")
                .iter()
                .any(|t| t.record == "HOLDER" && !t.passive_only),
            "the Db shape is only sound because the edge lands in the LOCAL CP \
             registry, where the post gate drives it"
        );
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
        db.get_record("IN1").unwrap().write().common.inp = "UP:ZED CP".to_string();
        db.get_record("IN2").unwrap().write().common.inp = "ca://UP:ZED CP".to_string();
        // An OUT link reaches `dbCaAddLink` in C exactly as an IN link does.
        db.get_record("OUT1").unwrap().write().common.out = "ca://UP:ALPHA".to_string();
        // A link to a record this IOC does have is local and must not count.
        db.get_record("LOCAL_HOLDER").unwrap().write().common.inp = "LOCAL_SRC CP".to_string();

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
    use crate::server::database::PvDatabase;
    use crate::server::record::{ParsedLink, parse_link_v2};
    use crate::types::EpicsValue;
    use std::collections::HashSet;
    use std::sync::Arc;

    /// C `dbInitLink` (`dbLink.c:118-128`, the call being `dbCaAddLink` at
    /// `:128`) makes a PV link whose target is
    /// not a local record a CA link — even with NO `CP`/`CPP`/`CA`
    /// modifier (`dbDbInitLink` fails to resolve it locally and falls
    /// through to `dbCaAddLink`). So a plain `INP="OTHER:PV"`
    /// to a non-local target must read the remote value, not return
    /// `None`. Pre-fix the `Db` read arm read only the local DB (`get_pv`)
    /// and dropped the non-local target.
    #[epics_macros_rs::epics_test]
    async fn plain_nonlocal_db_link_reads_via_external_resolver() {
        let db = PvDatabase::new();
        // Stand-in for the calink/pvalink lset: resolve OTHER:PV remotely.
        db.set_external_resolver(Arc::new(|name: &str| {
            (name == "OTHER:PV").then_some(EpicsValue::Double(42.0))
        }))
        .await;

        let link = parse_link_v2("OTHER:PV");
        assert!(
            matches!(link, ParsedLink::Db(_)),
            "a bare non-scheme name parses to a Db link, got {link:?}"
        );

        let mut visited = HashSet::new();
        let v = db.read_link_value(&link, &mut visited, 0);
        assert_eq!(
            v,
            Some(EpicsValue::Double(42.0)),
            "a plain non-local Db link must resolve through the external resolver (C dbCaAddLink fallback)"
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
        db.set_external_resolver(Arc::new(|name: &str| {
            (name == "OTHER:PV").then_some(EpicsValue::Double(5.0))
        }))
        .await;

        let link = parse_link_v2("OTHER:PV");
        let (fetch, alarm) = db.read_link_with_alarm(&link);
        assert_eq!(
            fetch.value(),
            Some(EpicsValue::Double(5.0)),
            "non-local Db link value must come from the external resolver"
        );
        // No lset is registered (only the legacy resolver), so the bare
        // external alarm path reports None — never a fabricated local
        // record alarm for a record that does not exist in this IOC.
        assert!(
            alarm.is_none(),
            "non-local Db link must not fabricate a local-record alarm, got {alarm:?}"
        );
    }

    /// Owner path: a Db link whose target IS a local record still reads
    /// the local database and never consults the external resolver.
    #[epics_macros_rs::epics_test]
    async fn local_db_link_reads_local_not_external() {
        let db = PvDatabase::new();
        db.add_pv("SRC", EpicsValue::Double(7.0)).await.unwrap();
        // A resolver returning a DIFFERENT value proves the local read
        // wins and the external path is not taken for a local target.
        db.set_external_resolver(Arc::new(|_name: &str| Some(EpicsValue::Double(-1.0))))
            .await;

        let link = parse_link_v2("SRC");
        let mut visited = HashSet::new();
        let v = db.read_link_value(&link, &mut visited, 0);
        assert_eq!(
            v,
            Some(EpicsValue::Double(7.0)),
            "a local Db link must read the local DB, not the external resolver"
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
    use std::collections::HashSet;

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
        let mut visited = HashSet::new();
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

        let mut visited = HashSet::new();
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

        let mut visited = HashSet::new();
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

        let mut visited = HashSet::new();
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
        let mut visited = HashSet::new();
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
        visited.insert("SRC".to_string());
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
        let mut visited = HashSet::new();
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
    use crate::server::database::link_set::{LinkPutOp, LinkSet, PutAdmission};
    use crate::types::EpicsValue;

    /// Connected, writable, and willing — so the only thing that can refuse
    /// the put is the missing reactor.
    struct WillingLset;

    #[async_trait::async_trait]
    impl LinkSet for WillingLset {
        fn is_connected(&self, _: &str) -> bool {
            true
        }
        fn put_admission(&self, _: &str) -> PutAdmission {
            PutAdmission::Connected
        }
        async fn get_value(&self, _: &str) -> Option<EpicsValue> {
            None
        }
        async fn put_value(&self, _: &str, _: EpicsValue, _: LinkPutOp) -> Result<(), String> {
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
            .external_put_admitted("ca://SOME:PV")
            .expect_err("a database with no reactor must refuse the external put");
        assert!(
            err.contains("no tokio runtime"),
            "the refusal must name the missing reactor, got: {err}"
        );
    }
}
