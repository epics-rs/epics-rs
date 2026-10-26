use crate::error::CaResult;
use crate::server::record::{
    AlarmSeverity, CommonFields, ProcessAction, Record, RecordInstance, ScanType,
};

/// Which of C's three built-in soft-channel dset families a DTYP names.
///
/// The question every caller actually asks is *which flavour*, not "is it
/// soft": each of the three does something different with the link, and a
/// caller that answers only yes/no has to re-spell the distinction itself.
/// Three sites did, each with its own two-value expression, and
/// [`SoftDtyp::Async`] fell out of all three — the attach phase skipped its
/// device (soft), while the processing cycle and the output path deferred to a
/// device that therefore did not exist. Input records read 0 instead of the
/// link value and output records wrote nothing, with no alarm.
///
/// Matching on this enum is what keeps that closed: a fourth flavour is a
/// non-exhaustive `match`, which is a compile error rather than a silent zero.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SoftDtyp {
    /// `""` or `"Soft Channel"` — C's `devXxxSoft.c`. Puts VAL/OVAL on the
    /// link; the input `read_xxx` returns 2, "do not convert".
    Plain,
    /// `"Raw Soft Channel"` — C's `devXxxSoftRaw.c`, a DIFFERENT dset that
    /// puts RVAL (`devAoSoftRaw.c:44`) and whose input `read_xxx` returns 0
    /// so the record DOES run the RVAL→VAL convert.
    Raw,
    /// `"Async Soft Channel"` — C's `devXxxSoftCallback.c`. Same VALUES as
    /// [`SoftDtyp::Plain`], deferred: `write_ao` is `dbPutLinkAsync(out,
    /// DBR_DOUBLE, &oval, 1)` with a synchronous `dbPutLink` fallback when the
    /// link has no LSET (`devAoSoftCallback.c:41-54`), and `read_ai` returns 2
    /// on every terminal path (`devAiSoftCallback.c:167-216`) after
    /// `dbProcessNotify` on INP has completed. An output whose link takes the
    /// put asynchronously (CA, pva) stays PACT until the put's callback
    /// completes it; any other link puts synchronously.
    Async,
}

/// The soft-channel flavour `dtyp` names, or `None` when it names device
/// support that owns the transfer.
///
/// Only the four genuine base soft-channel DTYPs. Timestamp-producing DTYPs
/// are NOT soft channels — they are real device support that writes a resolved
/// time stamp into VAL, so they must reach the device-lookup path rather than
/// short-circuit here:
///  - "Soft Timestamp" (base `devTimestamp.c`) — served by the
///    pre-registered `builtin_devices::builtin_dynamic_factory`.
///  - "Sec Past Epoch" / "Time of Day" (epics-modules/std `devTimeOfDay.c`)
///    — served by `std_rs::std_device_supports()`; if the IOC has not
///    registered them they correctly warn as "no device support", not
///    silently no-op as a soft channel. base-rs must not special-case a
///    std-module DTYP (layering leak).
pub fn classify_soft(dtyp: &str) -> Option<SoftDtyp> {
    match dtyp {
        "" | "Soft Channel" => Some(SoftDtyp::Plain),
        "Raw Soft Channel" => Some(SoftDtyp::Raw),
        "Async Soft Channel" => Some(SoftDtyp::Async),
        _ => None,
    }
}

/// C `prec->dpvt` as the soft dsets use it, handed to the record's soft dset
/// entry ([`Record::soft_input_read`]) as C hands `prec` to `read_xxx`.
///
/// A device-link put replaces it whole (`dbAccess.c:1192`), so per-link soft
/// state lives here and not on the record.
#[derive(Debug, Default)]
pub enum Dpvt {
    /// NULL.
    #[default]
    Null,
    /// `devAiSoft.c:88`'s `prec->dpvt = &devAiSoft`: a read has completed.
    Read,
    /// The devPvt a `devXxxSoftCallback` `add_record` allocates
    /// (`devAiSoftCallback.c:104-125`); a put that re-runs it gets a new one.
    Callback(Box<CallbackPvt>),
    /// `devEpidSoftCallback.c:143-145`'s `pepid->pact = TRUE`: the CA
    /// readback trigger is in flight, and the callback pass runs the PID
    /// instead of firing it again.
    Triggered,
}

/// A `devXxxSoftCallback` `devPvt`; its notify is the record's async re-entry
/// token, cancelled when a device-link put retires it.
#[derive(Debug, Default)]
pub struct CallbackPvt {
    /// `devAiSoftCallback.c`'s `smooth`.
    smooth: bool,
    /// `buffer.value`, as the last read of one element left it; `None` is
    /// `add_record`'s calloc'd zero.
    buffer: Option<crate::types::EpicsValue>,
    /// The notified target as the completion's `getCallback` read it.
    notified: Option<Box<crate::server::database::NotifiedRead>>,
}

impl Dpvt {
    /// The `SMOO` blend's "VAL holds a previous reading": `devAiSoft`'s
    /// `prec->dpvt != NULL`, `devAiSoftCallback`'s `smooth`.
    #[inline(always)]
    pub fn smooth(&self) -> bool {
        match self {
            Self::Null | Self::Triggered => false,
            Self::Read => true,
            Self::Callback(pvt) => pvt.smooth,
        }
    }

    /// The read succeeded (`devAiSoft.c:88`, `devAiSoftCallback.c:194`).
    #[inline(always)]
    pub fn read_done(&mut self) {
        match self {
            Self::Callback(pvt) => pvt.smooth = true,
            other => *other = Self::Read,
        }
    }

    /// The read failed (`devAiSoft.c:92`, `devAiSoftCallback.c:182`).
    #[inline(always)]
    pub fn read_failed(&mut self) {
        match self {
            Self::Callback(pvt) => pvt.smooth = false,
            other => *other = Self::Null,
        }
    }

    /// What a SoftCallback `read_xxx` copies VAL from once `getCallback`'s
    /// `dbChannelGetField(.., &no_elements = 1, ..)` read `value` into the
    /// devPvt buffer (`devAiSoftCallback.c:56-65`): a read of no elements
    /// writes nothing, so VAL takes the last reading. Any other dpvt reads
    /// `value` itself.
    pub(crate) fn callback_read(
        &mut self,
        value: crate::types::EpicsValue,
    ) -> crate::types::EpicsValue {
        let Self::Callback(pvt) = self else {
            return value;
        };
        match value.first_element() {
            Some(read) => pvt.buffer.insert(read).clone(),
            None if value.is_array() => pvt.buffer.clone().unwrap_or_else(|| {
                crate::types::EpicsValue::from_bytes(value.db_field_type(), &[0; 8])
                    .unwrap_or(value)
            }),
            None => pvt.buffer.insert(value).clone(),
        }
    }

    /// Keep what the notify's callback read for the re-entry.
    pub(crate) fn set_notified_read(
        &mut self,
        read: Option<Box<crate::server::database::NotifiedRead>>,
    ) {
        if let Self::Callback(pvt) = self {
            pvt.notified = read;
        }
    }

    /// The re-entry's read; `None` once taken, or for a non-callback dpvt.
    pub(crate) fn take_notified_read(
        &mut self,
    ) -> Option<Box<crate::server::database::NotifiedRead>> {
        match self {
            Self::Callback(pvt) => pvt.notified.take(),
            _ => None,
        }
    }

    /// The address `dbpr` prints for a SoftCallback devPvt.
    pub(crate) fn callback(&self) -> Option<&CallbackPvt> {
        match self {
            Self::Callback(pvt) => Some(pvt),
            _ => None,
        }
    }
}

/// The soft dset `record_type`'s `dtyp` binds: [`classify_soft`]'s class,
/// unless the row the DTYP selects was registered with a device object —
/// mca's `devMCA_soft` is a `"Soft Channel"` the framework does not serve.
/// Asked at init and on a device-link put, never by the cycle.
pub fn served_soft(record_type: &str, dtyp: &str) -> Option<SoftDtyp> {
    let dtyp = crate::server::record::served_choice(record_type, dtyp);
    classify_soft(dtyp).filter(|_| !crate::server::record::has_device_object(record_type, dtyp))
}

/// Does this DTYP need no explicit device support registration?
///
/// The attach phase's question, and the only one that is genuinely yes/no:
/// all three flavours are served by the framework, so none of them looks up a
/// registered device.
pub fn is_soft_dtyp(dtyp: &str) -> bool {
    classify_soft(dtyp).is_some()
}

/// A record's `DTYP` together with the soft-channel class it names.
///
/// C reads `prec->dtyp` as an index into the record type's device menu and
/// never compares the string at process time; a link's `lset` was chosen at
/// iocInit. This port asks [`classify_soft`] instead, and a scan cycle asks it
/// three times — the input stage, the RVAL convert and the soft-output value
/// — so a `calc` that names no device support paid three out-of-line `bcmp`
/// calls per cycle to re-derive an answer fixed when DTYP was written.
///
/// The classification is stored WITH the name and the fields are private, so
/// the two cannot disagree before iocInit. `bind` then fixes it: C's
/// `dset` is chosen once from DTYP at init, and a later DTYP put stores the
/// menu index without touching the running record's dset.
#[derive(Clone, Debug)]
pub struct Dtyp {
    name: DtypName,
    class: DsetClass,
    /// [`Self::soft`], stored with the class: four classes make the match a
    /// branch tree, and the process cycle asks it on every record. Written
    /// only by [`Self::set_class`].
    soft: Option<SoftDtyp>,
    /// `soft()` is `Plain` or `Async`: the classes whose input dset returns 2.
    /// Stored with the class so the process cycle tests one byte; written only
    /// by [`Self::set_class`].
    skips_input_convert: bool,
    /// The `dsxt` of the dset bound at init or by the last device-link put,
    /// C's `dbDSETtoDevSup(prec->dset)->pdsxt`, which a later DTYP put does
    /// not change. Written by [`Self::bind`], [`Self::rebind`] and
    /// [`Self::detach`].
    bound_dsxt: BoundDsxt,
}

/// Whether the record's bound dset extends with a `dsxt`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum BoundDsxt {
    /// No dset (C `prec->dset == NULL`), or one no devSup row describes.
    Unknown,
    Has,
    Lacks,
}

/// The DTYP text. A device-menu choice is the menu's own `&'static str`, so a
/// DTYP put under the record's hold stores a pointer and frees nothing; only a
/// name no menu has is owned.
#[derive(Clone, Debug)]
pub(crate) enum DtypName {
    Menu(&'static str),
    Other(std::sync::Arc<str>),
}

impl std::ops::Deref for DtypName {
    type Target = str;

    fn deref(&self) -> &str {
        match self {
            Self::Menu(name) => name,
            Self::Other(name) => name,
        }
    }
}

/// Which soft dset a record runs, and whether DTYP may still change it.
#[derive(Clone, Copy, Debug)]
enum DsetClass {
    /// Before init: the class the current name would bind.
    Named(Option<SoftDtyp>),
    /// After init: the class the record was bound to.
    Bound(Option<SoftDtyp>),
    /// Bound to `Async`, whose `add_record` refused INP: C's `dpvt` stays
    /// NULL, so every `read_xxx` returns 2 without reading.
    AsyncRefused,
    /// C `prec->dset = NULL` after a device-link put whose new dset's
    /// `add_record` refused (`dbAccess.c:1194-1197`); the next device-link
    /// put binds again. Every arm that sets it also skips `dbAddLink`, so it
    /// is the device link's removed state too.
    Detached,
}

impl Dtyp {
    /// Classify `name` once and keep the answer beside it.
    pub fn new(name: impl Into<String>) -> Self {
        let name: String = name.into();
        let mut dtyp = Self {
            name: if name.is_empty() {
                DtypName::Menu("")
            } else {
                DtypName::Other(name.into())
            },
            class: DsetClass::Named(None),
            soft: None,
            skips_input_convert: false,
            bound_dsxt: BoundDsxt::Unknown,
        };
        dtyp.set_class(DsetClass::Named(classify_soft(&dtyp.name)));
        dtyp
    }

    /// The one writer of `class`, and so of the byte derived from it.
    fn set_class(&mut self, class: DsetClass) {
        self.class = class;
        self.soft = match class {
            DsetClass::Named(soft) | DsetClass::Bound(soft) => soft,
            DsetClass::AsyncRefused => Some(SoftDtyp::Async),
            DsetClass::Detached => None,
        };
        self.skips_input_convert = matches!(self.soft, Some(SoftDtyp::Plain | SoftDtyp::Async));
    }

    /// A put to DTYP. Before `bind` the class follows the name; after
    /// it, only the name changes.
    pub fn set_name(&mut self, name: impl Into<String>) {
        let name: String = name.into();
        self.store_name(DtypName::Other(name.into()));
    }

    /// [`Self::set_name`] for a device-menu choice.
    pub(crate) fn set_menu_name(&mut self, name: &'static str) {
        self.store_name(DtypName::Menu(name));
    }

    fn store_name(&mut self, name: DtypName) {
        self.name = name;
        if let DsetClass::Named(_) = self.class {
            self.set_class(DsetClass::Named(classify_soft(&self.name)));
        }
    }

    /// Fix the class at init, as C's `dbInitRecord` fixes `prec->dset`.
    pub(crate) fn bind(&mut self, record_type: &str) -> Option<SoftDtyp> {
        let soft = match self.class {
            DsetClass::Named(_) => return self.bind_dset(record_type),
            _ => self.soft(),
        };
        self.set_class(DsetClass::Bound(soft));
        soft
    }

    /// Bind the dset the current name selects.
    fn bind_dset(&mut self, record_type: &str) -> Option<SoftDtyp> {
        let soft = served_soft(record_type, &self.name);
        self.set_class(DsetClass::Bound(soft));
        self.bound_dsxt = match crate::server::record::dev_sup_of(record_type, self.as_str()) {
            Some(row) if row.has_dsxt() => BoundDsxt::Has,
            Some(_) => BoundDsxt::Lacks,
            None => BoundDsxt::Unknown,
        };
        soft
    }

    /// The bound dset's `dsxt` ([`Self::bound_dsxt`]).
    pub(crate) fn bound_dsxt(&self) -> BoundDsxt {
        self.bound_dsxt
    }

    /// C `dbPutFieldLink`'s device-link rebind (`dbAccess.c:1127-1199`): an
    /// INP/OUT put sets `prec->dset` from the CURRENT DTYP and calls the new
    /// dset's `add_record`, never its `init_record`. So the class follows the
    /// name again and stays bound; the caller must not run
    /// [`crate::server::record::Record::init_soft_dset`] for it.
    pub fn rebind(&mut self, record_type: &str) -> Option<SoftDtyp> {
        self.bind_dset(record_type)
    }

    /// C `dev<T>SoftCallback::add_record` failing: only a record bound to
    /// `Async` has that hook.
    pub(crate) fn refuse_link(&mut self) {
        if let DsetClass::Bound(Some(SoftDtyp::Async)) = self.class {
            self.set_class(DsetClass::AsyncRefused);
        }
    }

    /// C `dbPutFieldLink`'s failure arm: the record has no dset.
    pub(crate) fn detach(&mut self) {
        self.set_class(DsetClass::Detached);
        self.bound_dsxt = BoundDsxt::Unknown;
    }

    /// C `prec->dset == NULL` after `Self::detach`.
    pub fn is_detached(&self) -> bool {
        matches!(self.class, DsetClass::Detached)
    }

    /// The bound dset never reads INP (C `read_xxx` with a NULL `dpvt`).
    pub fn link_refused(&self) -> bool {
        matches!(self.class, DsetClass::AsyncRefused)
    }

    /// The DTYP text, as a client put it or the `.db` declared it.
    pub fn as_str(&self) -> &str {
        &self.name
    }

    /// [`Self::as_str`] as a handle that outlives the record's hold.
    pub(crate) fn shared_name(&self) -> DtypName {
        self.name.clone()
    }

    /// The soft-channel flavour the record runs — [`classify_soft`]'s answer
    /// for the name, frozen by `bind`.
    #[inline(always)]
    pub fn soft(&self) -> Option<SoftDtyp> {
        self.soft
    }

    /// `Plain` or `Async` soft channel — the input dset returns 2, never
    /// "Raw Soft Channel".
    #[inline(always)]
    pub fn skips_input_convert(&self) -> bool {
        self.skips_input_convert
    }

    /// [`is_soft_dtyp`] for a DTYP that already carries its class.
    pub fn is_soft(&self) -> bool {
        self.soft().is_some()
    }
}

impl Default for Dtyp {
    fn default() -> Self {
        Self::new("")
    }
}

impl From<&str> for Dtyp {
    fn from(name: &str) -> Self {
        Self::new(name)
    }
}

impl From<String> for Dtyp {
    fn from(name: String) -> Self {
        Self::new(name)
    }
}

impl std::fmt::Display for Dtyp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.name)
    }
}

impl PartialEq<str> for Dtyp {
    fn eq(&self, other: &str) -> bool {
        &*self.name == other
    }
}

impl PartialEq<&str> for Dtyp {
    fn eq(&self, other: &&str) -> bool {
        &*self.name == *other
    }
}

/// What [`DeviceSupport::write_begin`] did with the record's output.
///
/// Three answers, each its own variant: the two that end with the value at
/// the device — `Completed` and `Pending` — and the one that has not written
/// anything, `Synchronous`. The framework runs [`DeviceSupport::write`] only
/// on `Synchronous`; a device support that has already written must say
/// `Completed`, or the framework writes the same value a second time.
pub enum WriteStart {
    /// This device support has no submit-only path: the framework runs
    /// [`DeviceSupport::write`] now, under the record lock.
    Synchronous,
    /// The write ran to completion inside `write_begin` (a port that cannot
    /// block, C `ASYN_CANBLOCK` clear); nothing remains to be done.
    Completed,
    /// The write was submitted to a worker queue and the record stays PACT;
    /// the device ends the cycle through the source's
    /// [`DeviceDone`](crate::server::database::DeviceDone) when the write lands.
    Pending,
}

/// What [`DeviceSupport::read_begin`] did with the record's input.
pub enum ReadStart {
    /// No submit-only path this pass: the framework runs
    /// [`DeviceSupport::read`] now, under the record lock.
    Synchronous,
    /// The read was queued to a worker and the record stays PACT. The pass
    /// ends with no value, alarm, monitor or forward link (C `process()`'s
    /// `if (!pact && prec->pact) return 0`, aiRecord.c:149); the device
    /// re-enters it through the source's
    /// [`DeviceDone::reprocess`](crate::server::database::DeviceDone::reprocess),
    /// and that pass's [`DeviceSupport::read`] applies what was read.
    Pending,
    /// The completion already ran on this thread — C `queueRequest` on a port
    /// without `ASYN_CANBLOCK` runs `processUser` inline (asynManager.c:1555-
    /// 1584), and devMcaAsyn's callback calls `prset->process` from there
    /// (devMcaAsyn.c:357-382). The record is PACT; its re-entered pass runs
    /// nested now, monitors and FLNK included, and then this pass asks
    /// `read_begin` again with [`DeviceDoneSource::again`] set, as C's code
    /// after that `send_msg` may send the next one, until it answers
    /// `Synchronous` or `Pending`.
    ///
    /// [`DeviceDoneSource::again`]: crate::server::database::DeviceDoneSource::again
    Reentered,
    /// The device half reached the record's `readValue`, whose SIML and SIOL
    /// reads are link reads ([`Record::reads_simm_in_device_half`]). The
    /// framework makes them, raises their alarms, and asks again with
    /// [`DeviceDoneSource::sim_read`] set.
    ///
    /// [`Record::reads_simm_in_device_half`]: crate::server::record::Record::reads_simm_in_device_half
    /// [`DeviceDoneSource::sim_read`]: crate::server::database::DeviceDoneSource::sim_read
    ReadValue,
}

/// What C mca `readValue`'s link half decided (`mcaRecord.c:1106-1129`), for
/// the ask after a [`ReadStart::ReadValue`] answer.
#[derive(Debug, Clone, PartialEq)]
pub enum SimRead {
    /// SIMM is NO: the device reads.
    Device,
    /// SIMM is YES: what the SIOL read delivered, and whether SIOL is a DB
    /// link, the one kind whose count C takes as `nord` (`:1120`).
    Siol {
        fetch: crate::server::recgbl::simm::LinkFetch,
        db_link: bool,
    },
    /// `readValue` returned non-zero before any read: the SIML read failed,
    /// or SIMM is no menu choice. The alarm is raised.
    Failed,
}

/// What a device support `read()` produced.
///
/// This is one half of C's dset contract; [`DeviceUdf`] is the other. C's
/// `read_ai()` return value answers only "what did you write, and did you
/// write anything":
///
/// ```c
/// if (status==0) convert(prec);
/// else if (status==2) status=0;
/// if (status == 0) prec->udf = isnan(prec->val);
/// ```
///
/// What the record does about `prec->udf` afterwards is the record's own rule
/// and differs per type — `aiRecord.c:158-161` folds `2` into `0` before the
/// UDF line and so re-derives on both, while `biRecord.c:136-141` keeps its
/// assignment inside `if (status == 0)` and folds only afterwards, so a `2`
/// never reaches it. That is not a contradiction: a C dset returning `2` has already
/// written `prec->udf` itself (`devBiSoft.c:54-59`, `devBiDbState.c:67-70`,
/// `devMbbiSoft.c:55-60`, `devTimestamp.c:40-41`). Port device support cannot reach
/// `dbCommon`, so it states that fact through [`DeviceUdf`] instead and the
/// framework applies it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DeviceReadStatus {
    /// C `return 0` — device support wrote RVAL. The record runs its built-in
    /// conversion (ai: `ROFF → ASLO/AOFF → LINR/ESLO/EOFF → smoothing`).
    #[default]
    Converted,
    /// C `return 2` — device support wrote VAL directly, so the record skips
    /// its conversion and uses VAL as-is.
    ///
    /// **Common mistake:** returning [`Converted`](Self::Converted) when VAL is
    /// set directly lets the conversion overwrite VAL from RVAL (typically 0),
    /// making the read appear broken.
    Computed,
    /// C `return -1` and `return -2` — the read produced no value, so the
    /// record's previous VAL stands and no conversion runs.
    ///
    /// The two C returns differ only in what the dset wrote to `prec->udf`
    /// first — `processAiAverage` with `numAverage == 0` writes `prec->udf = 1`
    /// and returns `-2` (`devAsynInt32.c:900-904`), its transport-error branch
    /// writes nothing and returns `-1` (`:924-927`) — and at the record both
    /// miss `if (status == 0)` identically. So the UDF half lives in
    /// [`DeviceUdf`] and this variant carries only "nothing was produced";
    /// keeping two value-variants that differed by a UDF fact was what let a
    /// caller state a value outcome and a UDF outcome that disagreed.
    NoValue,
}

impl DeviceReadStatus {
    /// Whether the record must skip its built-in RVAL→VAL conversion.
    ///
    /// C runs `convert()` only for `return 0` (`aiRecord.c:159`).
    pub fn skips_conversion(self) -> bool {
        !matches!(self, Self::Converted)
    }

    /// Whether the read produced no value this cycle (C `-1` / `-2`).
    pub fn read_failed(self) -> bool {
        matches!(self, Self::NoValue)
    }
}

/// What device support wrote to `prec->udf`, which in C the dset does itself.
///
/// Every C dset that returns `2` writes this first — `devBiSoft.c:54-59` and
/// `devBiDbState.c:67-70` clear it, `devAsynInt32.c:900-904` sets it — and the
/// record's `process()` may then overwrite it by its own rule. Port device
/// support holds a `&mut dyn Record` and cannot reach `dbCommon`, so it says
/// what it meant and the framework stays the single owner of the transition.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DeviceUdf {
    /// The dset did not write `prec->udf`; the record's own rule decides.
    #[default]
    Untouched,
    /// C `prec->udf = FALSE` — the value this read left in the record is
    /// defined.
    Defined,
    /// C `prec->udf = TRUE` — the record's value is undefined.
    Undefined,
}

/// Result of a device support `read()` call.
///
/// Carries what the read produced ([`DeviceReadStatus`]), what it said about
/// UDF ([`DeviceUdf`]), and any side-effect actions (link writes, delayed
/// reprocess) for the framework to execute.
#[derive(Default)]
pub struct DeviceReadOutcome {
    /// Actions for the framework to execute (WriteDbLink, ReprocessAfter, etc.)
    pub actions: Vec<ProcessAction>,
    /// What the read produced — C's `read_ai()` return value.
    pub status: DeviceReadStatus,
    /// What the read said about `prec->udf`. Private so the two facts can only
    /// be paired through the constructors below, which is what keeps "I wrote
    /// VAL" from being stated without saying what that meant for UDF.
    udf: DeviceUdf,
}

impl DeviceReadOutcome {
    /// Device support wrote RVAL and said nothing about UDF; the record runs
    /// its conversion and its own UDF rule.
    ///
    /// C equivalent: `read_ai()` returns 0 without touching `prec->udf`
    /// (`devAiSoftRaw.c`).
    pub fn ok() -> Self {
        Self::default()
    }

    /// Device support wrote RVAL *and* wrote `prec->udf`.
    ///
    /// C equivalent: `devTimestamp.c:65-66` — `prec->udf = FALSE; return 0`.
    pub fn converted(udf: DeviceUdf) -> Self {
        Self {
            status: DeviceReadStatus::Converted,
            actions: Vec::new(),
            udf,
        }
    }

    /// Device support wrote VAL directly; the record skips its conversion.
    ///
    /// C equivalent: `read_ai()` returns 2. The [`DeviceUdf`] argument is not
    /// optional because in C it is not: every dset that returns `2` has just
    /// written `prec->udf`, and the record types whose `process()` leaves UDF
    /// to the dset (`biRecord.c:136-141` and its mbbi / mbbiDirect / longin /
    /// int64in twins) have nothing else to go on.
    pub fn computed(udf: DeviceUdf) -> Self {
        Self {
            status: DeviceReadStatus::Computed,
            actions: Vec::new(),
            udf,
        }
    }

    /// Shorthand for a computed read with actions.
    pub fn computed_with(udf: DeviceUdf, actions: Vec<ProcessAction>) -> Self {
        Self {
            status: DeviceReadStatus::Computed,
            actions,
            udf,
        }
    }

    /// The read produced no value; what happens to UDF is the argument.
    pub fn no_value(udf: DeviceUdf) -> Self {
        Self {
            status: DeviceReadStatus::NoValue,
            actions: Vec::new(),
            udf,
        }
    }

    /// The read produced no value and said nothing about UDF; the record's
    /// previous VAL and UDF both stand.
    ///
    /// C equivalent: `read_ai()` returns -1.
    pub fn failed() -> Self {
        Self::no_value(DeviceUdf::Untouched)
    }

    /// The read produced no value and the record's value is undefined.
    ///
    /// C equivalent: `read_ai()` returns -2, which every reference user pairs
    /// with `prec->udf = 1`.
    pub fn undefined() -> Self {
        Self::no_value(DeviceUdf::Undefined)
    }

    /// What this read said about `prec->udf`.
    pub fn udf(&self) -> DeviceUdf {
        self.udf
    }

    /// Whether the read declares the record's value undefined (C
    /// `prec->udf = 1`).
    pub fn asserts_undefined(&self) -> bool {
        matches!(self.udf, DeviceUdf::Undefined)
    }

    /// Whether the record must skip its built-in conversion — true for every
    /// status but [`DeviceReadStatus::Converted`].
    pub fn did_compute(&self) -> bool {
        self.status.skips_conversion()
    }
}

/// What the record's dset `init_record` did, as the record's own C
/// `init_record` tests it before seeding MLST/ALST/LALM.
///
/// The record types do not agree on what a failure skips: `bi`, `longin`,
/// `longout`, `int64in`, `int64out` and `ao` return before the seed, `bo`,
/// `mbbi`, `mbbo`, `mbbiDirect`, `mbboDirect` and `calcout` seed regardless,
/// and `ai` seeds only when there is no dset `init_record` at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DsetInit {
    /// The dset has no `init_record` entry (devTimestampAI).
    Absent,
    /// C status 0. `bo`, `mbbo`, `mbboDirect` and `ao` convert the RVAL the
    /// dset left and define the record; `mbbiDirect` derives B0-B1F.
    Ok,
    /// C status 2, "do not convert": the record keeps the VAL the dset left.
    DoNotConvert,
    /// Any other status: [`DeviceInitOutcome::Dead`] or an `Err` from
    /// [`DeviceSupport::init`].
    Failed,
}

/// Whether a device support's `init_record` left the record able to process.
///
/// C's `init_record` failure has two shapes and they are not the same record
/// afterwards:
///
/// * `recGblRecordError(status, prec, ...); return status` — the record is
///   flagged and still processes. `devBiDbState.c:28-31` rejects an illegal INP
///   this way, `devGeneralTime.c:60-63` an illegal record type, and
///   `iocInit.c::doInitRecord1` discards the status, so the record scans on.
///   That is [`Err`] from [`DeviceSupport::init`].
/// * the `bad:` arm — `pr->pact = 1; return -1`
///   (`devAsynXXXTimeSeries.h:118-120`, and with a LINK_ALARM in
///   `devAsynInt32.c:348-351` and its Float64/Int64/UInt32Digital twins). PACT
///   is set at init and nothing ever clears it, so `dbProcess` takes its
///   already-active branch (`dbAccess.c:536-556`) on every later entry: the
///   record is DEAD. That is [`Dead`](Self::Dead).
///
/// The distinction is user-visible, which is why it needs a type rather than a
/// severity: a dead waveform reads PACT=1 and BUSY=0 forever, `caput REC.RARM 1`
/// sets RPRO and processes nothing (`dbAccess.c:1267-1271`), and a *scanned*
/// dead record collects SCAN_ALARM/INVALID once `lcnt` passes MAX_LOCK.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeviceInitOutcome {
    /// C `return INIT_OK` / `return 0` — the record processes normally, and
    /// the record types that test the status convert the RVAL the dset left
    /// ([`DsetInit::Ok`]). The [`DeviceUdf`] is what the dset itself wrote to
    /// `prec->udf` (devAsynInt32.c `initLo`: `pr->udf = 0`).
    Ok(DeviceUdf),
    /// C `return INIT_DO_NOT_CONVERT` / `return 2` — the record keeps the VAL
    /// the dset left ([`DsetInit::DoNotConvert`]); the [`DeviceUdf`] as above
    /// (devAsynFloat64.c `initAo`: `pao->udf = 0`).
    DoNotConvert(DeviceUdf),
    /// C's `bad:` arm — `pr->pact = 1`. The framework sets PACT and never
    /// releases it, so the record never processes again.
    ///
    /// Device support prints its own diagnostic before returning this, exactly
    /// as C `errlogPrintf`s before `goto bad`. The alarm rides along because the
    /// C arms do not agree on one and none of them can be reconstructed later:
    /// `devAsynXXXTimeSeries.h:118-120` raises none, while `devAsynInt32.c:348-351`
    /// and `devAsynOctet.c::initCmdBuffer:632-636` call `recGblSetSevr(precord,
    /// LINK_ALARM, INVALID_ALARM)` on the record itself, at init, next to the
    /// `pact = 1`. A dead record never processes, so a per-read alarm channel
    /// can never deliver it — it has to be applied here or not at all.
    ///
    /// Build with [`DeviceInitOutcome::dead`] or
    /// [`DeviceInitOutcome::dead_with_alarm`].
    Dead {
        /// `(STAT, SEVR)` for the `recGblSetSevr` that precedes C's `pact = 1`,
        /// or `None` for a `bad:` arm that raises nothing.
        alarm: Option<(u16, AlarmSeverity)>,
    },
}

impl DeviceInitOutcome {
    /// C `return 0` with `prec->udf` untouched — every dset that does not
    /// test or write either.
    pub fn ok() -> Self {
        Self::Ok(DeviceUdf::Untouched)
    }

    /// C's bare `bad:` arm — `pr->pact = 1` with no `recGblSetSevr`
    /// (`devAsynXXXTimeSeries.h:118-120`).
    pub fn dead() -> Self {
        Self::Dead { alarm: None }
    }

    /// C's `bad:` arm with the `recGblSetSevr` that precedes it
    /// (`devAsynInt32.c:348-351`: `LINK_ALARM` / `INVALID_ALARM`).
    pub fn dead_with_alarm(stat: u16, sevr: AlarmSeverity) -> Self {
        Self::Dead {
            alarm: Some((stat, sevr)),
        }
    }
}

/// One out-of-band PROPERTY post from a device support: the fields it writes
/// and the one field it posts on.
///
/// The two are deliberately different sets. C's enum re-propagation callbacks
/// (`devAsynInt32.c:712-766`, `devAsynUInt32Digital.c:547-601`, asyn
/// `e2a281e2`) are three statements under one `dbScanLock`:
///
/// ```c
/// setEnums((char*)&pr->zrst, (int*)&pr->zrvl, &pr->zrsv, ...);
/// db_post_events(pr, &pr->val, DBE_PROPERTY);
/// ```
///
/// `setEnums` rewrites ZRST/ZRVL/ZRSV… in place and posts on none of them;
/// the single `db_post_events` names `&pr->val`, so it is the client
/// monitoring the PV itself that learns the choices moved and re-reads
/// `DBR_GR_ENUM`. Collapsing the two sets into one field list would post
/// every state field and nothing on VAL — the opposite of both halves.
///
/// Measured end to end against C `libca` clients (R7.0.10 host build), not
/// just at this layer — the `mbbi_enum_property_ioc` example in `epics-ca-rs`
/// is the IOC half. On the post, a `DBR_GR_ENUM` + `DBE_PROPERTY`
/// subscription (base's own attribute-re-read shape, `dbCa.c`) receives a
/// second event carrying `["OFF","ON","FAULT"]` with `value` unmoved at 1,
/// and `camonitor -m p` re-renders `One` as `ON`. A `camonitor -m va` on the
/// same record sees only its initial event, which is the discrimination this
/// type exists for: re-keyed labels are not a new reading.
#[derive(Debug, Clone)]
pub struct PropertyPost {
    /// Fields to store without posting.
    pub writes: Vec<(String, crate::types::EpicsValue)>,
    /// The field `db_post_events` names, posted `DBE_PROPERTY` after the
    /// writes land, under the same record lock.
    pub post_field: String,
}

/// One `db_post_events(precord, &field, mask)` a device makes itself, outside
/// the record's `monitor()`, with the value C's call copies into the event.
///
/// StreamDevice's output `readData` (`devaoStream.c:76-104`) posts from
/// inside the protocol, and later formats of the same protocol can change the
/// field before the post is delivered, so the value is read at the call.
#[derive(Debug, Clone)]
pub struct DevicePost {
    pub field: &'static str,
    pub mask: crate::server::recgbl::EventMask,
    pub value: crate::types::EpicsValue,
}

impl DevicePost {
    /// The posts `recGblResetAlarms` makes (`recGbl.c:202-217`) for a reset
    /// that returned `reset`, `common` as it left it.
    pub fn alarm_reset(
        common: &CommonFields,
        reset: &crate::server::recgbl::AlarmResetResult,
    ) -> Vec<DevicePost> {
        use crate::types::EpicsValue;
        let mut posts = Vec::new();
        crate::server::database::alarm_field_posts(common, reset).for_each(|desc, mask| {
            let value = match desc.name {
                "SEVR" => EpicsValue::Short(common.sevr as i16),
                "STAT" => EpicsValue::Short(common.stat as i16),
                "AMSG" => EpicsValue::String(String::from(&common.amsg).into()),
                _ => EpicsValue::Short(common.acks as i16),
            };
            posts.push(DevicePost {
                field: desc.name,
                mask,
                value,
            });
        });
        posts
    }
}

/// What a device's callback under its record's lock decided
/// ([`DeviceSupport::io_intr_callback`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IoIntrAction {
    /// `dbProcess` the record before the lock is let go (devMotorAsyn's
    /// `asynCallback`).
    Process,
    /// `scanOnce` the record once the lock is let go (devEtherIP's
    /// `check_ao_callback`).
    ScanOnce,
    /// Leave it.
    Skip,
}

/// Whether [`AsyncDbHandle::run_io_intr_callback_with`](crate::server::database::AsyncDbHandle::run_io_intr_callback_with)
/// ran the callback.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IoIntrRun {
    /// It ran under the record's lock.
    Ran,
    /// This thread was inside the record: it runs once the record is free.
    Owed,
    /// It never runs: before `finishDevSup` (C's `dbScanLockOK` still
    /// false), or with the record or the database gone.
    Refused,
}

/// C `get_ioint_info`'s `cmd`: which of `dbScan.c`'s two calls asks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IoIntrCmd {
    /// `scanAdd`, `cmd` 0: the record joins the I/O Intr list.
    Add = 0,
    /// `scanDelete`, `cmd` 1: the record leaves it.
    Delete = 1,
}

/// The alarm a device raises after a transfer: C's
/// `recGblSetSevrMsg(prec, stat, sevr, msg)`, with an empty `msg` for a
/// plain `recGblSetSevr`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DeviceAlarm<'a> {
    pub stat: u16,
    pub sevr: u16,
    pub msg: &'a str,
}

/// Trait for custom device support implementations.
/// When DTYP is set to something other than "" or "Soft Channel",
/// the registered DeviceSupport is used instead of link resolution.
pub trait DeviceSupport: Send + Sync + 'static {
    /// C `init_record`. See [`DeviceInitOutcome`] for the two failure shapes:
    /// `Err` flags the record but leaves it processing, `Ok(Dead)` is C's
    /// `pr->pact = 1` and stops it for good. The default is an `init_record`
    /// that returns 0, so the record's status-0 arm runs; a dset with no
    /// `init_record` says so through [`Self::has_init_record`].
    fn init(&mut self, _record: &mut dyn Record) -> CaResult<DeviceInitOutcome> {
        Ok(DeviceInitOutcome::ok())
    }

    /// Whether the C dset this ports has an `init_record` entry. `aiRecord.c`
    /// seeds MLST/ALST/LALM from VAL only when it does not.
    fn has_init_record(&self) -> bool {
        true
    }

    /// Whether the C dset this ports fills ai/ao's `special_linconv` slot.
    /// The presence alone matters: `aiRecord.c:188-191` rebases EOFF to EGUL
    /// before the call, so a dset whose routine does nothing still moves EOFF.
    fn has_special_linconv(&self) -> bool {
        false
    }

    /// C `special_linconv(prec, after)`: recompute ESLO/EOFF from EGUL/EGUF.
    /// Called on both `special()` passes of a LINR/EGUF/EGUL put while LINR is
    /// LINEAR, and only when [`Self::has_special_linconv`] says the slot is
    /// filled. The record never calls it from `init_record`; a dset that wants
    /// the slope at init computes it in its own [`Self::init`], as C does.
    fn special_linconv(&mut self, _record: &mut dyn Record, _after: bool) -> CaResult<()> {
        Ok(())
    }

    /// C's `dsxt->add_record(precord)` for a device that extends its dset
    /// (`devExtend`): bind the per-link state from the link this object was
    /// built with. A device-link put runs it under the record's hold on the
    /// fresh object, which already has its record info, and fails on `Err`
    /// (`dbAccess.c:1196-1199`); init discards the status as `doResolveLinks`
    /// does.
    fn add_record(&mut self, _record: &mut dyn Record) -> Result<(), crate::error::LinkRefusal> {
        Ok(())
    }

    /// C's `dsxt->del_record` on the bound object, run under the record's
    /// hold when a device-link put passes the `dsxt` gate. `Err` refuses the
    /// put with the link, dset and SCAN as they were (`dbAccess.c:1160-1164`).
    fn del_record(&mut self) -> Result<(), crate::error::LinkRefusal> {
        Ok(())
    }

    /// Whether the C dset this ports leaves `prec->dpvt` set: this object
    /// then stands for it, and `dbpr` prints its address.
    fn holds_dpvt(&self) -> bool {
        false
    }

    /// Read from hardware into the record.
    ///
    /// Returns a `DeviceReadOutcome` containing:
    /// - `actions`: side-effect actions (link writes, delayed reprocess)
    ///   that the framework will execute after process()
    /// - `did_compute`: if true, the record's built-in compute was already
    ///   performed (e.g., device support ran PID), so process() should skip it
    fn read(&mut self, record: &mut dyn Record) -> CaResult<DeviceReadOutcome> {
        let _ = record;
        Ok(DeviceReadOutcome::ok())
    }

    fn write(&mut self, record: &mut dyn Record) -> CaResult<()>;
    fn dtyp(&self) -> &str;

    /// Return the alarm the driver's last transfer left.
    /// None means the driver does not override alarms.
    fn last_alarm(&self) -> Option<DeviceAlarm<'_>> {
        None
    }

    /// Return the last timestamp from the driver.
    /// None means the driver does not override timestamps.
    fn last_timestamp(&self) -> Option<crate::types::WallTime> {
        None
    }

    /// Return the userTag the driver attached to its reading, as the
    /// 64-bit `epicsUTag`. `None` means the driver provides no userTag
    /// and `common.utag` is left untouched.
    ///
    /// This is the channel a timing receiver (event system) uses to
    /// deliver a pulse-id / event tag: `epicsTimeStamp` itself carries
    /// no tag and the generalTime event path (`epicsTimeGetEvent`)
    /// delivers only the timestamp, so the tag must come through device
    /// support — mirroring C device support writing `prec->utag`
    /// directly during `read()` (alongside `prec->time`, TSE=-2).
    fn last_utag(&self) -> Option<u64> {
        None
    }

    /// Called by the framework immediately before [`read()`](DeviceSupport::read)
    /// and every write to push a read-only snapshot of framework-owned
    /// `CommonFields` state ([`crate::server::record::ProcessContext`]) that
    /// the device support needs.
    ///
    /// `read()` receives only `&mut dyn Record`; it cannot reach
    /// `RecordInstance.common`. C device support reads `dbCommon`
    /// directly — `devTimeOfDay.c:122` selects its time format from
    /// `psi->phas`. A driver that needs `phas`/`udf`/`tse`/`tsel`
    /// overrides this to stash the values before `read()` runs.
    ///
    /// Additive framework-set-hook (same shape as
    /// [`DeviceSupport::set_record_info`]). Default: ignore.
    fn set_process_context(&mut self, _ctx: &crate::server::record::ProcessContext) {}

    /// Called after init() with the record name and scan type.
    fn set_record_info(&mut self, _name: &str, _scan: ScanType) {}

    /// The callback band the record's `PRIO` selects, handed over just
    /// before [`Self::init`]: C device and record support read `prec->prio`
    /// in `init_record` for the callbacks they set up there
    /// (motorRecord.cc:634). Default: ignore.
    fn set_callback_priority(&mut self, _prio: crate::runtime::task::CallbackPriority) {}

    /// Forward parsed `info("key", "value")` directives from the .db
    /// file to the device support. Default is a no-op; drivers that
    /// react to specific tags (asyn `asyn:READBACK`, EtherCAT terminal
    /// hints, etc.) override this. Called once after `set_record_info`
    /// during builder wiring; not called again at runtime.
    fn apply_record_info(&mut self, _info: &std::collections::HashMap<String, String>) {}

    /// Tell the device which database it is loaded in, before
    /// [`io_intr_receiver`](Self::io_intr_receiver) is asked: the gate is that
    /// database's `interruptAccept`, for a device that must read it itself.
    /// It does not choose the port's gate — a port reads its owner's alone.
    /// Default: ignore.
    fn bind_interrupt_gate(
        &mut self,
        _gate: &std::sync::Arc<crate::runtime::interrupt_accept::InterruptGate>,
    ) {
    }

    /// Tell the device which database's `finishDevSup` gate it is under,
    /// before [`io_intr_receiver`](Self::io_intr_receiver) is asked: the
    /// stand-in for a dset `init(after)` that latches the second pass.
    /// Default: ignore.
    fn bind_dev_sup_gate(
        &mut self,
        _gate: &std::sync::Arc<crate::runtime::interrupt_accept::InterruptGate>,
    ) {
    }

    /// Hand the device its record's database before [`Self::init`], for a
    /// device that reads other records itself, as devAsynOctet's
    /// `asynOctetWriteRead` reads its command record (`initDbAddr`,
    /// devAsynOctet.c:645-657). Default: ignore.
    fn bind_database(&mut self, _db: crate::server::database::AsyncDbHandle) {}

    /// Return a receiver for I/O Intr scan notifications.
    /// Called for records with `SCAN="I/O Intr"`, and for any device that
    /// reports [`io_intr_scan_independent`](Self::io_intr_scan_independent).
    fn io_intr_receiver(&mut self) -> Option<crate::runtime::sync::mpsc::Receiver<()>> {
        None
    }

    /// The sweeps the device's driver queued at the interrupt edge and has
    /// not run, C asynPortDriver's `callbackThread`: a local CP link's first
    /// event on an I/O Intr record waits until none is owed, so it reads
    /// what the sweep posted. Default: `None`, a driver with no such sweep.
    fn edge_sweeps(&self) -> Option<std::sync::Arc<crate::runtime::interrupt_accept::Outstanding>> {
        None
    }

    /// Whether this device posts monitors itself: after `init_common`,
    /// `get_ioint_info`, `io_intr_callback` and `write_begin_common` the
    /// record's holder posts
    /// [`Self::take_device_posts`] while it still holds the record, before
    /// the record's own `monitor()`. A process cycle reads it when the
    /// record's cycle plan is resolved, so `false` costs a cycle nothing.
    fn posts_from_device(&self) -> bool {
        false
    }

    /// The posts this device made in the call that just returned, in order.
    fn take_device_posts(&mut self) -> Vec<DevicePost> {
        Vec::new()
    }

    /// Whether this device drives record processing from its own callback
    /// channel independently of the runtime `SCAN` menu.
    ///
    /// C parity: a `motorRecord` device callback (`statusCallback`) does its
    /// own `dbScanLock` + `dbProcess` on every poll readback regardless of
    /// `SCAN`, and the record stays `SCAN="Passive"` so a `dbPutField` to a
    /// `pp(TRUE)` field (VAL/DVAL/...) still re-processes it
    /// (`dbAccess.c:1263-1268`). asyn readback records behave the same way
    /// (upstream PRs #60/#208 — output records follow driver-side changes
    /// regardless of `SCAN`).
    ///
    /// When `true`, the I/O Intr wiring processes the record on every pulse
    /// even when `SCAN != "I/O Intr"`. When `false` (default), processing is
    /// gated on the record's current `SCAN` being `"I/O Intr"`, matching C
    /// `scanIoRequest`, which honors scan-list membership.
    fn io_intr_scan_independent(&self) -> bool {
        false
    }

    /// The device's own code under its record's lock, on the driver thread
    /// that calls [`AsyncDbHandle::run_io_intr_callback`](crate::server::database::AsyncDbHandle::run_io_intr_callback):
    /// devEtherIP's `check_ao_callback` copies the PLC's value into the record
    /// and calls `scanOnce` only when it changed, and not at all when the get
    /// failed (devEtherIP.c:326-437). Default: [`IoIntrAction::Skip`].
    fn io_intr_callback(
        &mut self,
        _record: &mut dyn Record,
        _common: &mut crate::server::record::CommonFields,
    ) -> IoIntrAction {
        IoIntrAction::Skip
    }

    /// C's dset `get_ioint_info(cmd, precord, &piosh)`. `scanAdd` asks it
    /// before it files the record on the list the call returns, `scanDelete`
    /// before it takes the record off (dbScan.c:285, :343), each under the
    /// record's lock set; the SCAN the record has is the one being joined or
    /// left. `false` is C's nonzero return: `scanAdd` leaves the record
    /// Passive. Default: accept, the pulse source being
    /// [`io_intr_receiver`](Self::io_intr_receiver).
    fn get_ioint_info(
        &mut self,
        _record: &mut dyn Record,
        _common: &mut CommonFields,
        _cmd: IoIntrCmd,
    ) -> bool {
        true
    }

    /// Whether a pulse stands for the device's own `dbProcess` rather than
    /// `scanIoRequest`: devMotorAsyn's `statusCallback` gates on the
    /// `dbScanLockOK` it latched at `interruptAccept` (devMotorAsyn.c:159-162,
    /// 756), so an `iocPause`, which only stops `scanIoRequest`
    /// (`dbScan.c:621`), still lets it process. Default: `false`.
    fn io_intr_processes_directly(&self) -> bool {
        false
    }

    /// Arm an output driver-callback (`asyn:READBACK`) cycle.
    ///
    /// Called by [`crate::server::database::PvDatabase::process_record_readback`]
    /// under the pass's record gate, before the pass, mirroring C
    /// `devAsynInt32.c::outputCallbackCallback` setting
    /// `newOutputCallbackValue = 1` before `dbProcess`. Pair with
    /// [`Self::reconcile_readback_callback`]: if the pass never reaches the
    /// device read stage (the PACT entry guard bails because a put / FLNK
    /// cycle still owns the record), the armed flag survives and reconcile
    /// discards the stale callback-ring entry so a callback ring never
    /// desyncs from the record's pop count. Default no-op — only output
    /// callback-driven device support (asyn readback) needs it.
    ///
    /// Returns the device's own write of UDF at this point, the one owner of
    /// it on a readback cycle: devAsyn clears it before `dbProcess`
    /// (`devAsynInt32.c:631` and its twins), devOpcua's write dset only in
    /// its read, after `checkAlarms`.
    fn arm_readback_callback(&mut self) -> DeviceUdf {
        DeviceUdf::Untouched
    }

    /// Reconcile an armed output driver-callback cycle after processing.
    ///
    /// C `outputCallbackCallback` fallback (devEpics `devAsynInt32.c`): after
    /// `dbProcess`, if `newOutputCallbackValue` is still set the record did
    /// not process, so `getCallbackValue` is called to drop the stale ring
    /// entry. Default no-op; see [`Self::arm_readback_callback`].
    fn reconcile_readback_callback(&mut self) {}

    /// C's second `write_xxx` call: the pass that completes a
    /// [`WriteStart::Pending`] write runs it where `writeValue` runs, so
    /// IVOA can skip it (`boRecord.c:221-245`), and before `monitor()`. It
    /// takes the write's result for [`Self::last_alarm`], raised next, may
    /// set the record's fields (`devMW100_bo.c:192-200` drops a trigger's VAL to 0), and
    /// requests a driver callback devAsyn deferred while the write held the
    /// record (`devAsynInt32.c:1013-1018`). Default no-op.
    fn async_write_completed(&mut self, _record: &mut dyn Record) {}

    /// Whether a driver-callback (`asyn:READBACK`) processing cycle replaces
    /// this device's output stage with a value readback.
    ///
    /// C devEpics (`devAsynInt32.c::processAo`/`processBo`/…) takes the
    /// `newOutputCallbackValue` readback branch on a callback cycle and never
    /// calls the output `write()` — re-writing would re-assert the setpoint
    /// and re-trigger the driver (the AD `Acquire` loop). Only device support
    /// implementing that contract (the [`Self::arm_readback_callback`] /
    /// [`Self::reconcile_readback_callback`] pair) returns `true`.
    ///
    /// Default `false`: a C `dbProcess` driven by a driver callback is a full
    /// record process, output stage included. `devMotorAsyn` has no readback
    /// suppression — the motor record dispatches its retry, backlash-leg,
    /// NTM-stop, and queued-motion-resume commands from exactly these
    /// CALLBACK_DATA passes, and suppressing the write strands them in the
    /// command mailbox (DMOV stuck 0, MIP=RETRY|MOVE, later puts time out).
    fn output_callback_readback(&self) -> bool {
        false
    }

    // C's dset entries take `dbCommon*`. The framework calls only the
    // `_common` forms below; each forwards to the form without `common` for
    // a device that does not reach the record's common fields.

    /// [`init`](Self::init) with the record's `dbCommon`.
    fn init_common(
        &mut self,
        record: &mut dyn Record,
        _common: &mut CommonFields,
    ) -> CaResult<DeviceInitOutcome> {
        self.init(record)
    }

    /// [`read_begin`](Self::read_begin) with the record's `dbCommon`.
    fn read_begin_common(
        &mut self,
        record: &mut dyn Record,
        _common: &mut CommonFields,
        done: &crate::server::database::DeviceDoneSource<'_>,
    ) -> CaResult<ReadStart> {
        self.read_begin(record, done)
    }

    /// [`read`](Self::read) with the record's `dbCommon`.
    fn read_common(
        &mut self,
        record: &mut dyn Record,
        _common: &mut CommonFields,
    ) -> CaResult<DeviceReadOutcome> {
        self.read(record)
    }

    /// [`read_common`](Self::read_common) for a device that
    /// [`reads_own`](Self::reads_own): the record with the
    /// [`OwnFrame`](crate::server::record::OwnFrame) its holder lends, so a
    /// `dbGet` the read makes of another record still answers, while it
    /// waits, a read of this one.
    fn read_own(
        &mut self,
        record: &mut dyn Record,
        _own: crate::server::record::OwnFrame<'_>,
    ) -> CaResult<DeviceReadOutcome> {
        self.read(record)
    }

    /// The cycle calls [`read_own`](Self::read_own) in place of
    /// [`read_common`](Self::read_common). Asked when the record's plan is
    /// made. Default `false`.
    fn reads_own(&self) -> bool {
        false
    }

    /// [`write_begin`](Self::write_begin) with the record's `dbCommon`.
    fn write_begin_common(
        &mut self,
        record: &mut dyn Record,
        _common: &mut CommonFields,
        done: &crate::server::database::DeviceDoneSource<'_>,
    ) -> CaResult<WriteStart> {
        self.write_begin(record, done)
    }

    /// [`write`](Self::write) with the record's `dbCommon`.
    fn write_common(
        &mut self,
        record: &mut dyn Record,
        _common: &mut CommonFields,
    ) -> CaResult<()> {
        self.write(record)
    }

    /// The record is lent to `init`, `read_begin`, `write_begin` and
    /// `scanAdd`'s `get_ioint_info` as a
    /// [`HeldInstance`](crate::server::database::HeldInstance) rather than
    /// borrowed: the framework lets the borrow go around each call and calls
    /// its `_held` form. For device support that puts to fields or processes
    /// records from inside those calls, as StreamEpics.cc's `matchValue`
    /// does. Asked when the device is attached. Default `false`.
    fn lends_held(&self) -> bool {
        false
    }

    /// [`init_common`](Self::init_common) for a device that
    /// [`lends_held`](Self::lends_held), the record held a step at a time.
    fn init_held(
        &mut self,
        hold: &mut crate::server::database::InitHold<'_>,
    ) -> CaResult<DeviceInitOutcome> {
        hold.held(|held| held.with(|record, common| self.init_common(record, common)))
    }

    /// [`read_begin_common`](Self::read_begin_common) for a device that
    /// [`lends_held`](Self::lends_held).
    fn read_begin_held(
        &mut self,
        held: &crate::server::database::HeldInstance<'_>,
        done: &crate::server::database::DeviceDoneSource<'_>,
    ) -> CaResult<ReadStart> {
        held.with(|record, common| self.read_begin_common(record, common, done))
    }

    /// [`write_begin_common`](Self::write_begin_common) for a device that
    /// [`lends_held`](Self::lends_held).
    fn write_begin_held(
        &mut self,
        held: &crate::server::database::HeldInstance<'_>,
        done: &crate::server::database::DeviceDoneSource<'_>,
    ) -> CaResult<WriteStart> {
        held.with(|record, common| self.write_begin_common(record, common, done))
    }

    /// `scanAdd`'s [`get_ioint_info`](Self::get_ioint_info) for a device
    /// that [`lends_held`](Self::lends_held); `scanDelete`'s is not lent.
    fn get_ioint_info_add_held(
        &mut self,
        held: &crate::server::database::HeldInstance<'_>,
    ) -> bool {
        held.with(|record, common| self.get_ioint_info(record, common, IoIntrCmd::Add))
    }

    /// [`async_write_completed`](Self::async_write_completed) with the
    /// record's `dbCommon`.
    fn async_write_completed_common(
        &mut self,
        record: &mut dyn Record,
        _common: &mut CommonFields,
    ) {
        self.async_write_completed(record)
    }

    /// Begin the record's input read — C devAsyn's `queueRequest` on a
    /// `canBlock` port with `pr->pact = 1` (devAsynInt32.c:838-842). Asked on
    /// every pass; on the pass a [`ReadStart::Pending`] completion re-entered,
    /// [`DeviceDoneSource::pact`](crate::server::database::DeviceDoneSource::pact)
    /// is set, and a device that reads what landed answers `Synchronous`.
    /// The default has no submit-only path.
    fn read_begin(
        &mut self,
        _record: &mut dyn Record,
        _done: &crate::server::database::DeviceDoneSource<'_>,
    ) -> CaResult<ReadStart> {
        Ok(ReadStart::Synchronous)
    }

    /// Begin the record's output write — see [`WriteStart`] for the three
    /// outcomes. The default has no submit-only path and asks for
    /// [`write()`](DeviceSupport::write).
    fn write_begin(
        &mut self,
        _record: &mut dyn Record,
        _done: &crate::server::database::DeviceDoneSource<'_>,
    ) -> CaResult<WriteStart> {
        Ok(WriteStart::Synchronous)
    }

    /// Handle a named command from the record's process() via
    /// `ProcessAction::DeviceCommand`. This allows records to request
    /// driver operations (e.g., scaler reset/arm/write_preset) without
    /// holding a direct driver reference.
    ///
    /// `handle_command` runs AFTER the process snapshot has already been
    /// built and notified, so any record field it mutates would not be
    /// diffed by the snapshot path. The returned `Vec` names the record
    /// fields the command changed; the framework posts a `DBE_VALUE`
    /// monitor event for each, mirroring the explicit `db_post_events`
    /// calls a C record makes from inside `process()` (e.g.
    /// `scalerRecord.c:425-430` posts PR1/TP/FREQ after the driver
    /// write-back). Return an empty `Vec` when no record field changed.
    ///
    /// Default: ignore, no fields changed.
    fn handle_command(
        &mut self,
        _record: &mut dyn Record,
        _command: &str,
        _args: &[crate::types::EpicsValue],
    ) -> CaResult<Vec<&'static str>> {
        Ok(Vec::new())
    }
}

/// Canonical device-support init sequence — the single owner of the
/// "attach device support to a record" contract.
///
/// Both build paths (`crate::server::ioc_app::wire_device_support`
/// and [`crate::server::ioc_builder::IocBuilder::build`]) MUST call
/// this so a driver author can write one correct `init()`.
///
/// Order (C parity — `recGblInitConstantLink`-style field setup runs
/// before `init_record`; `set_record_info` / `apply_record_info` are
/// Rust extensions that supply that field context and therefore
/// precede `init`):
///
/// 1. `set_record_info(name, scan)` — give the driver its record
///    identity and scan mode.
/// 2. `apply_record_info(info)` — forward `info(...)` tags so a
///    driver that reads them inside `init()` sees a populated map.
/// 3. `init(record)` — driver `init_record` equivalent.
///
/// On `init()` failure the record is flagged `INVALID` severity with
/// a `SOFT` status and a diagnostic is logged, so the failure is
/// observable rather than silently attached as healthy.
///
/// On [`DeviceInitOutcome::Dead`] this is also the single owner of
/// C's `pr->pact = 1` (`devAsynXXXTimeSeries.h:118-120`): the record
/// enters PACT here and nothing releases it, because the only release
/// is [`RecordInstance::leave_pact`] at a process-cycle tail and
/// `dbProcess`'s already-active guard turns every entry away before
/// the cycle starts. Device support cannot reach `dbCommon` through
/// the `&mut dyn Record` it holds, so it says *dead* and the framework
/// performs the transition — the same split as
/// [`DeviceUdf::Undefined`] and the UDF assertion.
///
/// Success clears NOTHING. In C, whether an `init_record` defines the
/// record is a property of the individual dset, not of the framework:
/// `devTimestamp.c` declares no `init_record` at all and clears
/// `prec->udf` only inside `read_ai`/`read_stringin` (`:40`, `:65`),
/// and `iocInit.c::doInitRecord0` (`:508-533`) only READS `udf` to
/// derive the initial severity. A record whose device support has
/// produced no value is still undefined, and says so.
///
/// The device is attached (`instance.device = Some(dev)`) regardless
/// of init outcome so the record is addressable; a failed init leaves
/// the alarm set.
///
/// A device that [`DeviceSupport::lends_held`] is lent `rec` for its
/// init, as the init passes lend it, so `rec` must not be borrowed here.
pub fn wire_device_to_record(
    db: &crate::server::database::PvDatabase,
    rec: &std::sync::Arc<crate::server::record::RecordCell>,
    dev: Box<dyn DeviceSupport>,
) {
    let mut instance = rec.write();
    let record_type = instance.record.record_type();
    instance.common.dtyp.bind(record_type);
    attach_device_to_record(&mut instance, dev);
    let owed = init_device_support(&mut instance, InitStage::Wire);
    let name = instance.name.to_str_lossy().into_owned();
    drop(db.pay_device_init(rec, instance, owed, &name, &|line| {
        crate::stderr_print!("{line}")
    }));
}

/// The BIND half of [`wire_device_to_record`]: C `iocInit.c::doInitRecord0`'s
/// `precord->dset = pdevSup ? pdevSup->pdset : NULL` (`:530-533`), which
/// happens BEFORE `prset->init_record(precord, 0)` and runs no driver code.
///
/// Split from the init half because C's two halves sit on opposite sides of
/// `init_record`: every `<rec>Record.c init_record` opens by testing the dset
/// this line bound (`if (!pdset) return S_dev_noDSET`) and only later calls
/// `pdset->common.init_record`. Attaching and initialising in one step is what
/// put the whole sequence after the record's init passes, so `init_record`
/// could not see its own dset and ran the tail C's early return skips.
pub fn attach_device_to_record(instance: &mut RecordInstance, mut dev: Box<dyn DeviceSupport>) {
    // Device supports take a `&str` name; one that is not UTF-8 reaches
    // them in its lossy form.
    let name = instance.name.to_str_lossy().into_owned();
    dev.set_record_info(&name, instance.common.scan);
    dev.apply_record_info(instance.info());
    instance.device = Some(dev);
    // `CycleFacts::device_posts` is the device's.
    instance.invalidate_plan();
}

/// Which init leaves an [`OwedDeviceInit`]: pass 1's tail runs after it.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum InitStage {
    Pass0,
    Pass1,
    /// [`wire_device_to_record`], outside the passes.
    Wire,
}

/// The dset `init_record` an init pass leaves to its caller for a device
/// that [`DeviceSupport::lends_held`]: called with the record's borrow let
/// go, by [`crate::server::database::PvDatabase::pay_device_init`].
#[must_use]
pub(crate) struct OwedDeviceInit {
    dev: Box<dyn DeviceSupport>,
    saved: Option<(f64, f64)>,
    stage: InitStage,
}

impl OwedDeviceInit {
    pub(crate) fn init_held(
        &mut self,
        hold: &mut crate::server::database::InitHold<'_>,
    ) -> CaResult<DeviceInitOutcome> {
        self.dev.init_held(hold)
    }

    /// The rest of [`init_device_support`], then the rest of the pass.
    pub(crate) fn finish(
        self,
        instance: &mut RecordInstance,
        outcome: CaResult<DeviceInitOutcome>,
        name: &str,
        err: &dyn Fn(&str),
    ) {
        finish_device_init(instance, self.dev, self.saved, outcome);
        if self.stage == InitStage::Pass1 {
            instance.finish_init_pass1(name, err);
        }
    }
}

/// The INIT half: C `pdset->common.init_record(prec)`, the call every record
/// type makes from inside its own `init_record` once the dset test above has
/// passed (`aiRecord.c:115-124`). A device that lends held is left owed, for
/// [`crate::server::database::PvDatabase::pay_device_init`].
///
/// No-op for a record that has no device attached — C reaches this line only
/// past `if (!pdset) return S_dev_noDSET`.
pub(crate) fn init_device_support(
    instance: &mut RecordInstance,
    stage: InitStage,
) -> Option<OwedDeviceInit> {
    let (mut dev, saved) = begin_device_init(instance)?;
    if dev.lends_held() {
        return Some(OwedDeviceInit { dev, saved, stage });
    }
    let outcome = dev.init_common(&mut *instance.record, &mut instance.common);
    finish_device_init(instance, dev, saved, outcome);
    None
}

/// [`init_device_support`] up to the dset call: the device and the saved
/// EOFF/ESLO, or `None` once a record with no device is done.
fn begin_device_init(
    instance: &mut RecordInstance,
) -> Option<(Box<dyn DeviceSupport>, Option<(f64, f64)>)> {
    let saved = instance.record.eoff_eslo_before_dset_init();
    if let Some(soft) = instance.common.dtyp.soft() {
        instance.record.init_soft_dset(soft);
    }
    let Some(mut dev) = instance.device.take() else {
        let row = crate::server::record::dev_sup_of(
            instance.record.record_type(),
            instance.common.dtyp.as_str(),
        );
        if let Some(saved) = saved.filter(|_| row.is_some_and(|r| r.has_init_record())) {
            instance.record.eoff_eslo_after_dset_init(saved);
        }
        instance.record.after_dset_init();
        instance.dset_init = if instance.common.dtyp.is_soft() {
            instance.record.soft_dset_init()
        } else {
            DsetInit::Ok
        };
        return None;
    };
    hand_device_its_record(instance, &mut *dev);
    Some((dev, saved))
}

/// What a dset object reads through `prec` in C before its `init_record`, or
/// a device-link put's `add_record` (`dbAccess.c:1192-1196`): its database
/// and the PRIO band.
pub(crate) fn hand_device_its_record(instance: &RecordInstance, dev: &mut dyn DeviceSupport) {
    dev.bind_database(instance.device_done.database());
    dev.set_callback_priority(instance.common.callback_priority());
}

/// [`init_device_support`] after the dset call.
fn finish_device_init(
    instance: &mut RecordInstance,
    mut dev: Box<dyn DeviceSupport>,
    saved: Option<(f64, f64)>,
    outcome: CaResult<DeviceInitOutcome>,
) {
    instance.post_device_values(dev.take_device_posts());
    instance.record.after_dset_init();
    if let Some(saved) = saved.filter(|_| dev.has_init_record()) {
        instance.record.eoff_eslo_after_dset_init(saved);
    }
    let status = match outcome {
        Ok(DeviceInitOutcome::Ok(udf)) => {
            apply_init_udf(instance, udf);
            // The record's own `if (status == 0)` arm, after the dset's write;
            // a dset with no `init_record` never reaches it.
            if dev.has_init_record() {
                if let Some(udf) = instance.record.dset_init_convert() {
                    instance.common.udf = udf as u8;
                }
            }
            DsetInit::Ok
        }
        Ok(DeviceInitOutcome::DoNotConvert(udf)) => {
            apply_init_udf(instance, udf);
            DsetInit::DoNotConvert
        }
        Ok(DeviceInitOutcome::Dead { alarm }) => {
            // C's `bad:` arm. The driver has already printed why. The
            // `recGblSetSevr` comes first in every C arm that has one, so it
            // runs before PACT here too — and through the same helper, so a
            // record already carrying a higher severity keeps it.
            if let Some((stat, sevr)) = alarm {
                crate::server::recgbl::rec_gbl_set_sevr(&mut instance.common, stat, sevr);
            }
            instance.enter_pact();
            DsetInit::Failed
        }
        // iocInit.c:570 discards init_record's status: the dset printed its
        // own line, and what the failure means to the record is
        // `DsetInit::Failed`, not an alarm.
        Err(_) => DsetInit::Failed,
    };
    if status == DsetInit::Failed
        && let Some(message) = instance.record.bad_init_ret_message()
    {
        crate::server::recgbl::rec_gbl_record_error(
            &crate::server::recgbl::DevSupStatus::BadInitRet.text(),
            &instance.name.to_str_lossy(),
            message,
        );
    }
    instance.dset_init = if !dev.has_init_record() {
        DsetInit::Absent
    } else {
        status
    };
    instance.device = Some(dev);
}

/// The dset's own write of `prec->udf` from inside its `init_record`.
fn apply_init_udf(instance: &mut RecordInstance, udf: DeviceUdf) {
    match udf {
        DeviceUdf::Untouched => {}
        DeviceUdf::Defined => instance.common.udf = 0,
        DeviceUdf::Undefined => instance.common.udf = 1,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::CaError;
    use crate::server::database::PvDatabase;
    use crate::server::record::{Record, RecordCell, RecordInstance, ScanType};
    use crate::server::records::ai::AiRecord;
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};

    /// Observed wiring state, shared with the test via `Arc` so it is
    /// inspectable after the device is moved into the record.
    #[derive(Default)]
    struct WireObservation {
        /// Info keys visible to `init()`.
        info_at_init: Vec<String>,
        /// Whether `set_record_info` ran before `init()`.
        record_info_before_init: bool,
        /// Whether `set_record_info` had run by the time `init` ran.
        init_ran: bool,
        /// The band `set_callback_priority` handed over before `init`.
        prio_at_init: Option<crate::runtime::task::CallbackPriority>,
    }

    /// What the probe's `init` returns — the three C shapes.
    #[derive(Clone, Copy)]
    enum InitVerdict {
        /// C `return INIT_OK`.
        Live,
        /// C's `bad:` arm — `pr->pact = 1`.
        Dead,
        /// C `recGblRecordError(status, ...); return status`, PACT untouched.
        Fail,
    }

    /// Device support that records the wiring order and returns `verdict`
    /// from `init`.
    struct ProbeDev {
        obs: Arc<Mutex<WireObservation>>,
        info: HashMap<String, String>,
        record_info_set: bool,
        prio: Option<crate::runtime::task::CallbackPriority>,
        verdict: InitVerdict,
    }
    impl DeviceSupport for ProbeDev {
        fn dtyp(&self) -> &str {
            "ProbeDev"
        }
        fn write(&mut self, _record: &mut dyn Record) -> CaResult<()> {
            Ok(())
        }
        fn set_record_info(&mut self, _name: &str, _scan: ScanType) {
            self.record_info_set = true;
        }
        fn apply_record_info(&mut self, info: &HashMap<String, String>) {
            self.info = info.clone();
        }
        fn set_callback_priority(&mut self, prio: crate::runtime::task::CallbackPriority) {
            self.prio = Some(prio);
        }
        fn init(&mut self, _record: &mut dyn Record) -> CaResult<DeviceInitOutcome> {
            let mut o = self.obs.lock().unwrap();
            o.init_ran = true;
            o.prio_at_init = self.prio;
            o.record_info_before_init = self.record_info_set;
            o.info_at_init = self.info.keys().cloned().collect();
            match self.verdict {
                InitVerdict::Live => Ok(DeviceInitOutcome::ok()),
                InitVerdict::Dead => Ok(DeviceInitOutcome::dead()),
                InitVerdict::Fail => Err(CaError::InvalidValue("device init failed".into())),
            }
        }
    }

    /// Wire `dev` onto a fresh ai `name`, which `set` prepares first.
    async fn wired(
        name: &str,
        set: impl FnOnce(&mut RecordInstance),
        dev: ProbeDev,
    ) -> (PvDatabase, Arc<RecordCell>) {
        let db = PvDatabase::new();
        db.add_record(name, Box::new(AiRecord::new(0.0)))
            .await
            .unwrap();
        let rec = db.get_record(name).unwrap();
        {
            let mut instance = rec.write();
            instance.common.dtyp = "ProbeDev".into();
            set(&mut instance);
        }
        wire_device_to_record(&db, &rec, Box::new(dev));
        (db, rec)
    }

    /// Wire a probe with `verdict` onto a fresh ai.
    async fn wire(verdict: InitVerdict) -> (PvDatabase, Arc<RecordCell>) {
        let dev = ProbeDev {
            obs: Arc::new(Mutex::new(WireObservation::default())),
            info: HashMap::new(),
            record_info_set: false,
            prio: None,
            verdict,
        };
        wired("TEST:DEAD", |_| {}, dev).await
    }

    /// Device support reads the record's `PRIO` band in `init_record`, as C's
    /// motor record does for its delay callback (motorRecord.cc:634).
    #[epics_macros_rs::epics_test]
    async fn init_sees_the_band_of_the_record_s_prio() {
        use crate::runtime::task::CallbackPriority;
        for (prio, band) in [
            (0, CallbackPriority::Low),
            (1, CallbackPriority::Medium),
            (2, CallbackPriority::High),
        ] {
            let obs = Arc::new(Mutex::new(WireObservation::default()));
            let dev = ProbeDev {
                obs: obs.clone(),
                info: HashMap::new(),
                record_info_set: false,
                prio: None,
                verdict: InitVerdict::Live,
            };
            wired("TEST:PRIO", |instance| instance.common.prio = prio, dev).await;
            assert_eq!(obs.lock().unwrap().prio_at_init, Some(band), "PRIO {prio}");
        }
    }

    /// C's `bad:` arm sets `pr->pact = 1` and nothing ever clears it, so
    /// `dbProcess` takes its already-active branch forever
    /// (`devAsynXXXTimeSeries.h:118-120`, `dbAccess.c:536`). The port had no way
    /// for device support to reach that state: an invalid FTVL left the driver
    /// returning an inert `read()` while the record kept processing with PACT=0.
    #[epics_macros_rs::epics_test]
    async fn wire_device_dead_init_leaves_the_record_in_pact() {
        let (_db, rec) = wire(InitVerdict::Dead).await;
        let instance = rec.read();

        assert!(
            instance.is_processing(),
            "C `bad: pr->pact = 1` — the record must be dead"
        );
        assert!(
            instance.device.is_some(),
            "a dead record is still addressable; only processing stops"
        );
    }

    /// The framework must not invent an alarm for the dead arm: the
    /// `devAsynXXXTimeSeries.h` `bad:` label raises none (its whole body is
    /// `pr->pact=1; return -1`), and the arms that do raise one
    /// (`devAsynInt32.c:349` LINK_ALARM) do it from device support, through the
    /// driver's own alarm channel.
    #[epics_macros_rs::epics_test]
    async fn wire_device_dead_init_raises_no_alarm_of_its_own() {
        let (_dead_db, dead) = wire(InitVerdict::Dead).await;
        let (_live_db, live) = wire(InitVerdict::Live).await;
        let (dead, live) = (dead.read(), live.read());

        assert_eq!(dead.common.sevr, live.common.sevr);
        assert_eq!(
            dead.common.stat, live.common.stat,
            "the dead arm leaves STAT at whatever the record was born with"
        );
    }

    /// The other two verdicts leave the record processing. `Err` is C's
    /// `recGblRecordError(status, prec, ...); return status` with PACT untouched
    /// (`devBiDbState.c:28-31`, `devGeneralTime.c:60-63`) — a flagged record
    /// still scans, which is why the dead arm needed a value of its own rather
    /// than folding into the error channel.
    #[epics_macros_rs::epics_test]
    async fn wire_device_live_and_failed_inits_leave_the_record_processing() {
        assert!(!wire(InitVerdict::Live).await.1.read().is_processing());
        assert!(
            !wire(InitVerdict::Fail).await.1.read().is_processing(),
            "an errored init flags the record but must not kill it"
        );
    }

    /// iocInit.c:570 discards init_record's status: a failed dset init raises
    /// no alarm (the dset printed its own line), and the record learns of it
    /// only through `DsetInit::Failed`.
    #[epics_macros_rs::epics_test]
    async fn wire_device_init_failure_raises_no_alarm_and_marks_the_dset_failed() {
        let (_failed_db, failed) = wire(InitVerdict::Fail).await;
        let (_live_db, live) = wire(InitVerdict::Live).await;
        let (failed, live) = (failed.read(), live.read());

        assert_eq!(failed.common.sevr, live.common.sevr);
        assert_eq!(failed.common.stat, live.common.stat);
        assert_eq!(failed.dset_init, DsetInit::Failed);
        assert!(
            failed.device.is_some(),
            "device is still attached so the record is addressable"
        );
    }

    /// M1 regression: the canonical wiring order is
    /// set_record_info → apply_record_info → init. A driver reading
    /// `info(...)` tags inside `init()` must see a populated map, and
    /// `set_record_info` must have run first.
    #[epics_macros_rs::epics_test]
    async fn wire_device_applies_info_and_record_info_before_init() {
        let obs = Arc::new(Mutex::new(WireObservation::default()));
        let dev = ProbeDev {
            obs: obs.clone(),
            info: HashMap::new(),
            record_info_set: false,
            prio: None,
            verdict: InitVerdict::Live,
        };

        let set_info = |instance: &mut RecordInstance| instance.set_info("asyn:READBACK", "1");
        wired("TEST:AI2", set_info, dev).await;

        let o = obs.lock().unwrap();
        assert!(o.init_ran, "init must have run");
        assert!(
            o.record_info_before_init,
            "set_record_info must run before init"
        );
        assert!(
            o.info_at_init.iter().any(|k| k == "asyn:READBACK"),
            "info(...) tags must be visible inside init()"
        );
    }

    /// The stored class must be the classifier's answer for every route into
    /// `Dtyp` — the whole point of keeping the two together is that a reader
    /// may trust `soft()` without re-deriving it.
    #[test]
    fn a_device_link_put_rebinds_to_the_current_dtyp_dbaccess_1128() {
        let mut dtyp = Dtyp::new("Soft Channel");
        assert_eq!(dtyp.bind("ai"), Some(SoftDtyp::Plain));
        dtyp.set_name("Raw Soft Channel");
        assert_eq!(dtyp.soft(), Some(SoftDtyp::Plain));
        assert!(dtyp.skips_input_convert());
        assert_eq!(dtyp.rebind("ai"), Some(SoftDtyp::Raw));
        assert_eq!(dtyp.soft(), Some(SoftDtyp::Raw));
        // The RVAL convert follows the rebound class, not the init one.
        assert!(!dtyp.skips_input_convert());
        // Still bound: the next DTYP put alone changes nothing again.
        dtyp.set_name("Soft Channel");
        assert_eq!(dtyp.soft(), Some(SoftDtyp::Raw));
        dtyp.set_name("asynInt32");
        assert_eq!(dtyp.rebind("ai"), None);
    }

    /// Only a bound `Async` dset has an `add_record` to fail, and an INP/OUT
    /// put's rebind fetches the dset afresh.
    #[test]
    fn only_a_bound_async_dset_refuses_its_link_devaisoftcallback_c_171() {
        let mut dtyp = Dtyp::new("Async Soft Channel");
        dtyp.refuse_link();
        assert!(!dtyp.link_refused());
        dtyp.bind("ai");
        dtyp.refuse_link();
        assert!(dtyp.link_refused());
        assert_eq!(dtyp.soft(), Some(SoftDtyp::Async));
        assert!(dtyp.skips_input_convert());
        assert_eq!(dtyp.rebind("ai"), Some(SoftDtyp::Async));
        assert!(!dtyp.link_refused());
        let mut plain = Dtyp::new("Soft Channel");
        plain.bind("ai");
        plain.refuse_link();
        assert!(!plain.link_refused());
    }

    #[test]
    fn a_dtyp_carries_the_class_its_name_classifies_to() {
        let names = [
            "",
            "Soft Channel",
            "Raw Soft Channel",
            "Async Soft Channel",
            "Soft Timestamp",
            "asynInt32",
            "Db State",
        ];
        for name in names {
            for built in [
                Dtyp::new(name),
                Dtyp::from(name),
                Dtyp::from(name.to_string()),
            ] {
                assert_eq!(built.as_str(), name);
                assert_eq!(built.soft(), classify_soft(name), "{name:?}");
                assert_eq!(built.is_soft(), is_soft_dtyp(name), "{name:?}");
                assert_eq!(
                    built.skips_input_convert(),
                    matches!(classify_soft(name), Some(SoftDtyp::Plain | SoftDtyp::Async)),
                    "{name:?}"
                );
                assert_eq!(built.clone().soft(), classify_soft(name), "{name:?} clone");
            }
        }
        assert_eq!(Dtyp::default().as_str(), "");
        assert_eq!(Dtyp::default().soft(), classify_soft(""));
    }
}
