use crate::error::CaResult;
use crate::server::record::{AlarmSeverity, ProcessAction, Record, RecordInstance, ScanType};

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
    /// `dbProcessNotify` has put the link's value straight into VAL. This port
    /// applies the link synchronously, so the observable difference is PACT
    /// timing, not the value.
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

/// Does this DTYP need no explicit device support registration?
///
/// The attach phase's question, and the only one that is genuinely yes/no:
/// all three flavours are served by the framework, so none of them looks up a
/// registered device.
pub fn is_soft_dtyp(dtyp: &str) -> bool {
    classify_soft(dtyp).is_some()
}

/// Handle for waiting on asynchronous write completion.
/// Returned by [`DeviceSupport::write_begin`] when the write is submitted
/// to a worker queue rather than executed synchronously.
pub trait WriteCompletion: Send + 'static {
    /// Block until the write completes or timeout expires.
    fn wait(&self, timeout: std::time::Duration) -> CaResult<()>;
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DeviceInitOutcome {
    /// C `return INIT_OK` / `return 0` — the record processes normally.
    #[default]
    Live,
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

/// Trait for custom device support implementations.
/// When DTYP is set to something other than "" or "Soft Channel",
/// the registered DeviceSupport is used instead of link resolution.
pub trait DeviceSupport: Send + Sync + 'static {
    /// C `init_record`. See [`DeviceInitOutcome`] for the two failure shapes:
    /// `Err` flags the record but leaves it processing, `Ok(Dead)` is C's
    /// `pr->pact = 1` and stops it for good.
    fn init(&mut self, _record: &mut dyn Record) -> CaResult<DeviceInitOutcome> {
        Ok(DeviceInitOutcome::Live)
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

    /// Return the last alarm (status, severity) from the driver.
    /// None means the driver does not override alarms.
    fn last_alarm(&self) -> Option<(u16, u16)> {
        None
    }

    /// Return the last timestamp from the driver.
    /// None means the driver does not override timestamps.
    fn last_timestamp(&self) -> Option<std::time::SystemTime> {
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
    /// to push a read-only snapshot of framework-owned `CommonFields`
    /// state ([`crate::server::record::ProcessContext`]) that the device
    /// support needs.
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

    /// Forward parsed `info("key", "value")` directives from the .db
    /// file to the device support. Default is a no-op; drivers that
    /// react to specific tags (asyn `asyn:READBACK`, EtherCAT terminal
    /// hints, etc.) override this. Called once after `set_record_info`
    /// during builder wiring; not called again at runtime.
    fn apply_record_info(&mut self, _info: &std::collections::HashMap<String, String>) {}

    /// Return a receiver for I/O Intr scan notifications.
    /// Called for records with `SCAN="I/O Intr"`, and for any device that
    /// reports [`io_intr_scan_independent`](Self::io_intr_scan_independent).
    fn io_intr_receiver(&mut self) -> Option<crate::runtime::sync::mpsc::Receiver<()>> {
        None
    }

    /// Return a receiver of out-of-band PROPERTY-class field posts.
    ///
    /// C parity: `registerInterruptUser(callbackEnum)` (devAsynInt32.c:319)
    /// plus the per-record enum callback
    /// (`interruptCallbackEnumMbbi`/`…Bi`, devAsynInt32.c:712-766), which
    /// calls `setEnums` to re-key the record's state strings/values/
    /// severities and then `db_post_events(precord, &precord->val,
    /// DBE_PROPERTY)` so CA/PVA clients re-read the enum choices. This is
    /// driven by the driver's `doCallbacksEnum`, independent of the
    /// record's `SCAN` (it is not a value scan, so it does not process the
    /// record).
    ///
    /// Each delivered message is one [`PropertyPost`]: the field block to
    /// write (the C `setEnums` block) and, separately, the single field
    /// `db_post_events` names. The framework drains this receiver and calls
    /// [`crate::server::database::PvDatabase::post_property`].
    /// Mirrors [`io_intr_receiver`](Self::io_intr_receiver): the device owns
    /// the source subscription, the framework owns the post. Default:
    /// `None` (device drives no property posts).
    fn property_post_receiver(
        &mut self,
    ) -> Option<crate::runtime::sync::mpsc::Receiver<PropertyPost>> {
        None
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

    /// Arm an output driver-callback (`asyn:READBACK`) cycle.
    ///
    /// Called by [`crate::server::database::PvDatabase::process_record_readback`]
    /// immediately before the processing pass, mirroring C
    /// `devAsynInt32.c::outputCallbackCallback` setting
    /// `newOutputCallbackValue = 1` before `dbProcess`. Pair with
    /// [`Self::reconcile_readback_callback`]: if the pass never reaches the
    /// device read stage (the PACT entry guard bails because a put / FLNK
    /// cycle still owns the record), the armed flag survives and reconcile
    /// discards the stale callback-ring entry so a callback ring never
    /// desyncs from the record's pop count. Default no-op — only output
    /// callback-driven device support (asyn readback) needs it.
    fn arm_readback_callback(&mut self) {}

    /// Reconcile an armed output driver-callback cycle after processing.
    ///
    /// C `outputCallbackCallback` fallback (devEpics `devAsynInt32.c`): after
    /// `dbProcess`, if `newOutputCallbackValue` is still set the record did
    /// not process, so `getCallbackValue` is called to drop the stale ring
    /// entry. Default no-op; see [`Self::arm_readback_callback`].
    fn reconcile_readback_callback(&mut self) {}

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

    /// Begin an asynchronous write (submit only, no blocking).
    /// Returns `Some(handle)` if the write was submitted to a worker queue —
    /// the caller should wait on the handle outside any record lock.
    /// Returns `None` to fall back to synchronous [`write()`](DeviceSupport::write).
    fn write_begin(
        &mut self,
        _record: &mut dyn Record,
    ) -> CaResult<Option<Box<dyn WriteCompletion>>> {
        Ok(None)
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
pub fn wire_device_to_record(instance: &mut RecordInstance, dev: Box<dyn DeviceSupport>) {
    attach_device_to_record(instance, dev);
    init_device_support(instance);
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
    let name = instance.name.clone();
    dev.set_record_info(&name, instance.common.scan);
    dev.apply_record_info(&instance.info);
    instance.device = Some(dev);
}

/// The INIT half: C `pdset->common.init_record(prec)`, the call every record
/// type makes from inside its own `init_record` once the dset test above has
/// passed (`aiRecord.c:115-124`).
///
/// No-op for a record that has no device attached — C reaches this line only
/// past `if (!pdset) return S_dev_noDSET`.
pub fn init_device_support(instance: &mut RecordInstance) {
    let Some(mut dev) = instance.device.take() else {
        return;
    };
    let name = instance.name.clone();
    match dev.init(&mut *instance.record) {
        Ok(DeviceInitOutcome::Live) => {}
        Ok(DeviceInitOutcome::Dead { alarm }) => {
            // C's `bad:` arm. The driver has already printed why. The
            // `recGblSetSevr` comes first in every C arm that has one, so it
            // runs before PACT here too — and through the same helper, so a
            // record already carrying a higher severity keeps it.
            if let Some((stat, sevr)) = alarm {
                crate::server::recgbl::rec_gbl_set_sevr(&mut instance.common, stat, sevr);
            }
            instance.enter_pact();
        }
        Err(e) => {
            eprintln!(
                "device support init failed for record '{name}' (DTYP '{}'): {e}",
                instance.common.dtyp
            );
            // Flag the record so the failure is observable rather
            // than presenting a healthy-looking record.
            instance.common.sevr = AlarmSeverity::Invalid;
            instance.common.stat = crate::server::recgbl::alarm_status::SOFT_ALARM;
        }
    }
    instance.device = Some(dev);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::CaError;
    use crate::server::record::{AlarmSeverity, Record, RecordInstance, ScanType};
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
        fn init(&mut self, _record: &mut dyn Record) -> CaResult<DeviceInitOutcome> {
            let mut o = self.obs.lock().unwrap();
            o.init_ran = true;
            o.record_info_before_init = self.record_info_set;
            o.info_at_init = self.info.keys().cloned().collect();
            match self.verdict {
                InitVerdict::Live => Ok(DeviceInitOutcome::Live),
                InitVerdict::Dead => Ok(DeviceInitOutcome::dead()),
                InitVerdict::Fail => Err(CaError::InvalidValue("device init failed".into())),
            }
        }
    }

    /// Wire a probe with `verdict` onto a fresh ai and hand back the instance.
    fn wire(verdict: InitVerdict) -> RecordInstance {
        let mut instance = RecordInstance::new("TEST:DEAD".to_string(), AiRecord::new(0.0));
        instance.common.dtyp = "ProbeDev".to_string();
        wire_device_to_record(
            &mut instance,
            Box::new(ProbeDev {
                obs: Arc::new(Mutex::new(WireObservation::default())),
                info: HashMap::new(),
                record_info_set: false,
                verdict,
            }),
        );
        instance
    }

    /// C's `bad:` arm sets `pr->pact = 1` and nothing ever clears it, so
    /// `dbProcess` takes its already-active branch forever
    /// (`devAsynXXXTimeSeries.h:118-120`, `dbAccess.c:536`). The port had no way
    /// for device support to reach that state: an invalid FTVL left the driver
    /// returning an inert `read()` while the record kept processing with PACT=0.
    #[test]
    fn wire_device_dead_init_leaves_the_record_in_pact() {
        let instance = wire(InitVerdict::Dead);

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
    #[test]
    fn wire_device_dead_init_raises_no_alarm_of_its_own() {
        let dead = wire(InitVerdict::Dead);
        let live = wire(InitVerdict::Live);

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
    #[test]
    fn wire_device_live_and_failed_inits_leave_the_record_processing() {
        assert!(!wire(InitVerdict::Live).is_processing());
        assert!(
            !wire(InitVerdict::Fail).is_processing(),
            "an errored init flags the record but must not kill it"
        );
    }

    /// M2 regression: a device support whose `init()` returns `Err`
    /// must NOT be attached as a healthy record — the record is
    /// flagged INVALID severity with a SOFT status. (Pre-fix the
    /// IocBuilder path discarded the error with `let _ =`.)
    #[test]
    fn wire_device_init_failure_flags_record_invalid() {
        let mut instance = RecordInstance::new("TEST:AI".to_string(), AiRecord::new(0.0));
        instance.common.dtyp = "ProbeDev".to_string();
        let obs = Arc::new(Mutex::new(WireObservation::default()));
        let dev = Box::new(ProbeDev {
            obs: obs.clone(),
            info: HashMap::new(),
            record_info_set: false,
            verdict: InitVerdict::Fail,
        });

        wire_device_to_record(&mut instance, dev);

        assert_eq!(
            instance.common.sevr,
            AlarmSeverity::Invalid,
            "failed device init must flag the record INVALID"
        );
        assert_eq!(
            instance.common.stat,
            crate::server::recgbl::alarm_status::SOFT_ALARM,
        );
        assert!(
            instance.device.is_some(),
            "device is still attached so the record is addressable"
        );
    }

    /// M1 regression: the canonical wiring order is
    /// set_record_info → apply_record_info → init. A driver reading
    /// `info(...)` tags inside `init()` must see a populated map, and
    /// `set_record_info` must have run first.
    #[test]
    fn wire_device_applies_info_and_record_info_before_init() {
        let mut instance = RecordInstance::new("TEST:AI2".to_string(), AiRecord::new(0.0));
        instance.common.dtyp = "ProbeDev".to_string();
        instance.set_info("asyn:READBACK", "1");
        let obs = Arc::new(Mutex::new(WireObservation::default()));
        let dev = Box::new(ProbeDev {
            obs: obs.clone(),
            info: HashMap::new(),
            record_info_set: false,
            verdict: InitVerdict::Live,
        });

        wire_device_to_record(&mut instance, dev);

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
}
