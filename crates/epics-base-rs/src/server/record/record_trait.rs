use crate::error::CaResult;
use crate::types::{DbFieldType, EpicsValue};

use super::scan::ScanType;

/// Metadata describing a single field in a record.
#[derive(Debug, Clone)]
pub struct FieldDesc {
    pub name: &'static str,
    pub dbf_type: DbFieldType,
    pub read_only: bool,
}

/// Per-field metadata deltas returned by
/// [`Record::field_metadata_override`].
///
/// Each `Some` member replaces the corresponding member of the
/// snapshot's record-level display/control metadata; `None` members
/// keep the record-level value.
#[derive(Debug, Clone, Default)]
pub struct FieldMetadataOverride {
    /// `display.units` — C RSET `get_units`.
    pub units: Option<crate::types::PvString>,
    /// `display.precision` — C RSET `get_precision`.
    pub precision: Option<i16>,
    /// `(upper, lower)` display limits — C RSET `get_graphic_double`.
    pub disp_limits: Option<(f64, f64)>,
    /// `(upper, lower)` control limits — C RSET `get_control_double`.
    pub ctrl_limits: Option<(f64, f64)>,
    /// `(hihi, high, low, lolo)` — C RSET `get_alarm_double`.
    pub alarm_limits: Option<(f64, f64, f64, f64)>,
}

/// Side-effect actions that a record requests from the processing framework.
///
/// Records return these from `process()` via `ProcessOutcome::actions`.
/// The framework executes them at the appropriate point in the processing
/// cycle, keeping records as pure state machines without direct DB access.
#[derive(Clone, Debug, PartialEq)]
pub enum ProcessAction {
    /// Write a value to a DB link. The framework reads `link_field` from the
    /// record to get the target PV name, then writes `value` to that PV.
    ///
    /// Executed after alarm/snapshot, before FLNK.
    /// Example: scaler writes CNT to COUT/COUTP links.
    WriteDbLink {
        link_field: &'static str,
        value: EpicsValue,
    },

    /// Read a value from a DB link into a record field. The framework reads
    /// `link_field` from the record to get the source PV name, reads that PV,
    /// and writes the result into `target_field` via an internal put that
    /// bypasses read-only checks.
    ///
    /// The value delivered is the link target's **native** [`EpicsValue`] — it
    /// is NOT coerced to a numeric type on the way in. The record coerces (or
    /// preserves) it at its own `put_field`/`put_field_internal` boundary, so a
    /// string-class source can reach a string field byte-exact (the `sseq`
    /// `DOLn`→`STRn` path, C `sseqRecord.c:643-705`). Records whose
    /// `target_field` is numeric simply convert there, exactly as before.
    ///
    /// **Pre-process action**: executed BEFORE the next process() cycle so
    /// the value is immediately available. This matches C EPICS `dbGetLink()`
    /// which is synchronous/immediate.
    ///
    /// Example: throttle reads SINP into VAL when SYNC is triggered.
    ReadDbLink {
        link_field: &'static str,
        target_field: &'static str,
    },

    /// Schedule a re-process of this record after the given duration.
    /// The framework spawns `tokio::spawn(sleep(d) + process_record(name))`.
    /// The current cycle's OUT/FLNK/notify proceed normally.
    ///
    /// Equivalent to C EPICS `callbackRequestDelayed()` + `scanOnce()`.
    ReprocessAfter(std::time::Duration),

    /// Send a named command to the device support driver.
    /// The framework calls `DeviceSupport::handle_command()` with this data.
    /// Used by scaler to request reset/arm/write_preset operations
    /// without the record holding a direct driver reference.
    DeviceCommand {
        command: &'static str,
        args: Vec<EpicsValue>,
    },

    /// Write a value to a DB link as a put-*with-completion*, then re-enter
    /// THIS record's `process()` when the downstream operation completes.
    ///
    /// The framework arms a put-notify wait-set (C `dbProcessNotify`),
    /// writes `link_field`'s target through it, releases the initiator's
    /// own count, and wires the completion to an async re-entry of this
    /// record (`mint_async_token` + `reprocess_on_notify`). The record
    /// returns [`RecordProcessResult::AsyncPending`] alongside this action
    /// and is re-entered once the downstream record (and its FLNK/OUT
    /// chain) finishes — the synApps `sseq` `WAITn` "wait for the put
    /// callback" dependency (`sseqRecord.c::processNextLink`,
    /// `dbCaPutLinkCallback`). Built on the same `new_put_notify` +
    /// `reprocess_on_notify` primitive an out-of-band
    /// [`crate::server::database::AsyncDbHandle`] caller uses.
    ///
    /// Executed before FLNK, like [`Self::WriteDbLink`].
    WriteDbLinkNotify {
        link_field: &'static str,
        value: EpicsValue,
    },

    /// Cancel this record's outstanding async re-entry (C
    /// `callbackCancelDelayed`): the framework advances the record's
    /// re-entry generation so any pending `ReprocessAfter` timer or
    /// `WriteDbLinkNotify` completion re-entry becomes a structural no-op
    /// (the `AsyncToken` gate), with no runtime "is-aborted" check on the
    /// re-entry path. Used by `sseq` `ABORT` to drop a pending `DLYn`
    /// delay or `WAITn` wait; the record resets its own sequence state in
    /// the same `process()` cycle that emits this.
    CancelReprocess,
}

/// Result of a record's process() call.
///
/// Determines how the framework handles the current processing cycle.
/// Side-effect actions (link writes, delayed reprocess, etc.) are expressed
/// separately in `ProcessOutcome::actions`.
#[derive(Clone, Debug, PartialEq)]
pub enum RecordProcessResult {
    /// Processing completed synchronously this cycle.
    /// Framework proceeds with alarm/timestamp/snapshot/OUT/FLNK.
    Complete,
    /// Processing started but not yet complete (PACT stays set).
    /// Current cycle skips alarm/timestamp/snapshot/OUT/FLNK.
    /// ProcessActions (if any) are still executed.
    AsyncPending,
    /// Async pending, but notify these intermediate field changes immediately.
    /// Used by motor records to flush DMOV=0 before the move completes.
    AsyncPendingNotify(Vec<(String, EpicsValue)>),
}

/// Complete outcome of a record's process() call.
///
/// Contains the processing result (Complete, AsyncPending, etc.) and a list
/// of side-effect actions for the framework to execute.
#[derive(Clone, Debug)]
pub struct ProcessOutcome {
    pub result: RecordProcessResult,
    pub actions: Vec<ProcessAction>,
    /// Set by the framework when device support's read() returned
    /// `did_compute: true`. The record's process() can check this to
    /// skip its built-in computation (e.g., PID). Replaces the `pid_done`
    /// flag pattern.
    pub device_did_compute: bool,
}

impl ProcessOutcome {
    /// Shorthand for a simple Complete with no actions.
    pub fn complete() -> Self {
        Self {
            result: RecordProcessResult::Complete,
            actions: Vec::new(),
            device_did_compute: false,
        }
    }

    /// Shorthand for Complete with actions.
    pub fn complete_with(actions: Vec<ProcessAction>) -> Self {
        Self {
            result: RecordProcessResult::Complete,
            actions,
            device_did_compute: false,
        }
    }

    /// Shorthand for AsyncPending with no actions.
    pub fn async_pending() -> Self {
        Self {
            result: RecordProcessResult::AsyncPending,
            actions: Vec::new(),
            device_did_compute: false,
        }
    }
}

impl Default for ProcessOutcome {
    fn default() -> Self {
        Self::complete()
    }
}

/// Result of setting a common field, indicating what scan index updates are needed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CommonFieldPutResult {
    NoChange,
    ScanChanged {
        old_scan: ScanType,
        new_scan: ScanType,
        phas: i16,
    },
    PhasChanged {
        scan: ScanType,
        old_phas: i16,
        new_phas: i16,
    },
}

/// Read-only snapshot of framework-owned `CommonFields` state that a
/// record's `process()` or device support's `read()` needs to see
/// *during* the processing cycle.
///
/// The framework owns `RecordInstance.common`; a record `process()`
/// receives only `&mut self` (the concrete record) and device support
/// `read()` receives only `&mut dyn Record`. Neither can reach
/// `CommonFields`. C records, by contrast, see `dbCommon` directly —
/// e.g. `epidRecord.c:195` reads `pepid->udf`, `timestampRecord.c:90`
/// reads `ptimestamp->tse`, `devTimeOfDay.c:122` reads `psi->phas`.
///
/// The framework builds a `ProcessContext` from `common` and pushes it
/// onto the record (via [`Record::set_process_context`]) and onto the
/// device support (via
/// [`crate::server::device_support::DeviceSupport::set_process_context`])
/// immediately before the respective call. This mirrors the existing
/// `set_device_did_compute` framework-set-hook pattern: additive,
/// no `process()` / `read()` signature change.
#[derive(Clone, Debug, PartialEq)]
pub struct ProcessContext {
    /// `dbCommon.udf` — value is undefined. C records check this at the
    /// top of `process()` (e.g. `epidRecord.c:195`).
    pub udf: bool,
    /// `dbCommon.udfs` — alarm severity raised for a UDF record.
    pub udfs: crate::server::record::AlarmSeverity,
    /// `dbCommon.phas` — phase. Used by device support for format
    /// selection (`devTimeOfDay.c:122`).
    pub phas: i16,
    /// `dbCommon.tse` — time-stamp event. `timestampRecord.c:90`
    /// branches on `tse == epicsTimeEventDeviceTime`.
    pub tse: i16,
    /// `dbCommon.time` — the record's current resolved time stamp at the
    /// start of this cycle (the previous cycle's stamp, or `UNIX_EPOCH`
    /// before the first process). Device support that has to format the
    /// record's time during `read()` — the std module's `devTimeOfDay.c`
    /// `recGblGetTimeStamp(psi)` call, which runs *before* the framework's
    /// per-cycle timestamp application — resolves the stamp with
    /// [`crate::server::recgbl::get_time_stamp`]`(tse, time)`. The `time`
    /// member is the device-provided value that helper returns verbatim on
    /// the `TSE == epicsTimeEventDeviceTime (-2)` branch.
    pub time: std::time::SystemTime,
    /// `dbCommon.tsel` — time-stamp event link string.
    pub tsel: String,
    /// `dbCommon.dtyp` — device-support type name. A record's
    /// `process()` / pre-process hooks can branch on the DTYP to mirror
    /// C device support that lives in a separate DSET (e.g. the epid
    /// record's `devEpidSoftCallback` callback DSET drives the TRIG
    /// readback link, whereas `devEpidSoft` does not).
    pub dtyp: String,
}

/// C `epicsTime.h`: `epicsTimeEventDeviceTime` — the `TSE` sentinel
/// meaning "device support provides the time stamp". `timestampRecord.c`
/// uses it to take the OS-clock branch instead of `recGblGetTimeStamp`.
pub const EPICS_TIME_EVENT_DEVICE_TIME: i16 = -2;

/// Snapshot of changes from a process cycle, used for notify outside lock.
pub struct ProcessSnapshot {
    /// `(field, value, mask)` — every posted field carries its own
    /// `DBE_*` posting mask, mirroring C's per-field
    /// `db_post_events(prec, &field, mask)`. One process cycle posts
    /// different classes per field: a deadband-gated readback narrows
    /// to the deadbands that actually crossed (MDEL → `DBE_VALUE`,
    /// ADEL → `DBE_LOG`; motorRecord.cc `monitor()` 3477-3507,
    /// aiRecord.c `monitor()`), while a change-detected auxiliary
    /// field posts `DBE_VALUE | DBE_LOG` (motorRecord.cc 3522-3645
    /// `DBE_VAL_LOG`; calcRecord.c:420). A single record-wide mask
    /// collapses that granularity — an archive-only deadband crossing
    /// would wrongly reach `DBE_VALUE` subscribers whenever any other
    /// field changed in the same pass.
    pub changed_fields: Vec<(String, EpicsValue, crate::server::recgbl::EventMask)>,
}

/// Trait that all EPICS record types must implement.
pub trait Record: Send + Sync + 'static {
    /// Return the record type name (e.g., "ai", "ao", "bi").
    fn record_type(&self) -> &'static str;

    /// Process the record (scan/compute cycle).
    ///
    /// Returns a `ProcessOutcome` containing the processing result and any
    /// side-effect actions for the framework to execute.
    fn process(&mut self) -> CaResult<ProcessOutcome> {
        Ok(ProcessOutcome::complete())
    }

    /// Optional: report whether this record's last `process()` call
    /// mutated a metadata-class field (EGU/PREC/HOPR/LOPR/HLM/LLM/
    /// alarm limits / DRVH/DRVL / state strings).
    ///
    /// The framework checks this after every `process()` call and, if
    /// true, invalidates the record's metadata cache so the next
    /// snapshot rebuilds from the new values.
    ///
    /// Default: `false` — most records never touch metadata fields
    /// during processing. Override only when your record dynamically
    /// adjusts limits or unit strings (e.g., a motor that recomputes
    /// HLM/LLM after a hardware homing operation).
    ///
    /// Implementations should reset their internal flag after returning
    /// `true` so the next cycle starts clean.
    fn took_metadata_change(&mut self) -> bool {
        false
    }

    /// Get a field value by name.
    fn get_field(&self, name: &str) -> Option<EpicsValue>;

    /// Set a field value by name.
    fn put_field(&mut self, name: &str, value: EpicsValue) -> CaResult<()>;

    /// Return the list of field descriptors.
    fn field_list(&self) -> &'static [FieldDesc];

    /// Choice strings for a record-specific `DBF_MENU` field served as
    /// `DBR_ENUM`, keyed by field name (uppercase, as declared).
    ///
    /// EPICS dbStaticLib serves a `DBF_MENU` field as `DBR_ENUM`: the value
    /// is the menu index and the field carries its `menu()` choice strings,
    /// so `caget`/`pvget` present the labels rather than a bare number
    /// (`dbStaticLib.c` `dbGetMenuChoices`; `dbAccess.c` `get_enum_str`).
    /// A record returns the label table (in index order) for each field it
    /// serves as [`DbFieldType::Enum`] from a `menu()`; the framework
    /// attaches it to the field snapshot's `EnumInfo` so the CA/PVA enum
    /// encoders present the labels — the same mechanism `bi`/`bo`/`mbbi`/
    /// `mbbo` already use for their `VAL` state strings, but per field
    /// rather than per record (a record can carry several distinct menus).
    ///
    /// This is the single owner of "menu field -> choice table": a record
    /// declares its menu fields here once, and `get_field` returns the menu
    /// index as [`EpicsValue::Enum`]. Default: no record-specific menu
    /// fields. The dbCommon menu fields (`SCAN`, etc.) are handled
    /// separately by the framework, not here.
    fn menu_field_choices(&self, _field: &str) -> Option<&'static [&'static str]> {
        None
    }

    /// Per-field override of the record-level display/control metadata
    /// for a GET / monitor snapshot of `field`.
    ///
    /// C record support serves metadata PER FIELD: the RSET functions
    /// `get_units` / `get_precision` / `get_graphic_double` /
    /// `get_control_double` / `get_alarm_double` all key on
    /// `dbGetFieldIndex(paddr)` and fall back to the `recGbl*` defaults
    /// for unlisted fields. The framework's metadata cache is per
    /// record (built by `populate_display_info` /
    /// `populate_control_info` from the VAL-class fields); a record
    /// whose RSET serves different metadata for non-VAL fields
    /// overrides this hook to patch the cached values for that field
    /// (e.g. the motor record: VELO's display range is VMAX/VBAS, not
    /// HLM/LLM — `motorRecord.cc:3247-3250`).
    ///
    /// Applied on both the GET path (`snapshot_for_field`) and the
    /// monitor path (`make_monitor_snapshot`), AFTER the cached
    /// record-level metadata — and computed live on each call, so an
    /// override derived from non-cached fields can never go stale.
    /// `field` is uppercase, as declared in [`Record::field_list`].
    /// Default: `None` — record-level metadata serves every field.
    fn field_metadata_override(&self, _field: &str) -> Option<FieldMetadataOverride> {
        None
    }

    /// Field names this record serves as a *long string*: a `DBF_CHAR`
    /// array field that semantically holds a NUL-terminated string.
    ///
    /// In EPICS such a field is declared `DBF_NOACCESS` (or carries a `$`
    /// modifier) and is accessed through a `DBR_CHAR` array view whose
    /// `form` is `"String"`; pvxs maps that view to a scalar `pvString`
    /// rather than an `int8[]` (`ioc/channel.cpp:58-68`,
    /// `ioc/iocsource.cpp:619-643`). QSRV uses this list to serve those
    /// fields as scalar-string NTScalar values instead of byte scalars.
    ///
    /// The record keeps its `CharArray` storage; the QSRV boundary does
    /// the `CharArray <-> String` conversion. Default empty — only
    /// long-string record types (`lsi`/`lso` VAL/OVAL, `printf` VAL)
    /// override this. Names are matched case-insensitively.
    fn long_string_fields(&self) -> &'static [&'static str] {
        &[]
    }

    /// Field names declared `pp(TRUE)` in this record type's DBD, or
    /// `None` if the type's pp-flags have not been modeled.
    ///
    /// Drives the `dbPutField` processing gate: C
    /// `dbAccess.c:1263` re-processes a record on a put only when the put
    /// field is `PROC` or it is `pp(TRUE)` **and** `SCAN == Passive`. A
    /// `None` return tells the put path to fall back to the legacy
    /// "process on every put" behavior, so un-modeled record types keep
    /// working unchanged. The default consults the central DBD-sourced
    /// table keyed by [`Record::record_type`]; record types can override.
    fn process_passive_fields(&self) -> Option<&'static [&'static str]> {
        super::process_passive::pp_fields_for(self.record_type())
    }

    /// Validate a put before it is applied. Return Err to reject.
    fn validate_put(&self, _field: &str, _value: &EpicsValue) -> CaResult<()> {
        Ok(())
    }

    /// Hook called after a successful put_field.
    fn on_put(&mut self, _field: &str) {}

    /// Primary field name (default "VAL"). Override for waveform etc.
    fn primary_field(&self) -> &'static str {
        "VAL"
    }

    /// Get the primary value.
    fn val(&self) -> Option<EpicsValue> {
        self.get_field(self.primary_field())
    }

    /// Set the primary value.
    ///
    /// Matches C EPICS `dbPut` behavior: if the value type doesn't match
    /// the field type, it is automatically coerced (e.g., Long→Double for
    /// ai, Long→Enum for bi/mbbi). This prevents silent failures when
    /// asyn device support provides Int32 values to Enum-typed records.
    fn set_val(&mut self, value: EpicsValue) -> CaResult<()> {
        let field = self.primary_field();
        match self.put_field(field, value.clone()) {
            Ok(()) => Ok(()),
            Err(crate::error::CaError::TypeMismatch(_)) => {
                // Auto-coerce: determine target type from current VAL
                let target_type = self
                    .get_field(field)
                    .map(|v| v.db_field_type())
                    .unwrap_or(DbFieldType::Double);
                let coerced = value.convert_to(target_type);
                self.put_field(field, coerced)
            }
            Err(e) => Err(e),
        }
    }

    /// Whether this record implements the `DTYP="Raw Soft Channel"`
    /// read path via [`Record::apply_raw_input`]. Records that return
    /// `true` opt into framework routing of the INP link value through
    /// `apply_raw_input` (RVAL + MASK) instead of the default
    /// soft-channel `VAL` direct write.
    ///
    /// Default `false` keeps any record that has not been wired for
    /// raw soft channel on the legacy path (which sets VAL directly).
    fn accepts_raw_soft_input(&self) -> bool {
        false
    }

    /// Apply a value read from a `DTYP="Raw Soft Channel"` INP link.
    ///
    /// Mirrors the C `devXxxSoftRaw.c` `read_xxx()` convention: the
    /// raw value goes to `RVAL` (so the record's `process()` then runs
    /// the standard `RVAL → VAL` conversion). Records that expose a
    /// `MASK` field must apply it here, matching epics-base
    /// `f2fe9d12` (devBiSoftRaw: `prec->rval &= prec->mask`).
    ///
    /// Only invoked by the framework when
    /// [`Record::accepts_raw_soft_input`] returns `true`.
    fn apply_raw_input(&mut self, value: EpicsValue) -> CaResult<()> {
        self.set_val(value)
    }

    /// Apply IVOA=2 ("set outputs to IVOV") semantics: copy the
    /// IVOV value into whatever output staging field the OUT
    /// writeback consumes for this record type. Mirrors the
    /// per-record C `recXxx.c` behaviour:
    ///
    /// - `ao`/`lso`: `OVAL = IVOV; VAL = OVAL`
    /// - `bo`/`busy`/`mbbo`/`mbboDirect`: `RVAL = IVOV; VAL = IVOV`
    /// - `calcout`/`scalcout`: `OVAL = IVOV` (VAL is calc input, not
    ///   touched on invalid-output)
    /// - `dfanout`: `VAL = IVOV` (the broadcast value)
    ///
    /// Default uses [`Record::set_val`] for records whose OUT path
    /// reads VAL only.
    fn apply_invalid_output_value(&mut self, ivov: EpicsValue) -> CaResult<()> {
        self.set_val(ivov)
    }

    /// Whether this record type supports device write (output records only).
    /// `aao` is included here even though it's served by the same
    /// concrete struct as `waveform`/`aai`/`subArray` — the
    /// WaveformRecord's `can_device_write` override picks the right
    /// answer per [`ArrayKind`], but this default matters for code that
    /// only has the record-type string.
    fn can_device_write(&self) -> bool {
        matches!(
            self.record_type(),
            "ao" | "bo"
                | "longout"
                | "int64out"
                | "mbbo"
                | "mbboDirect"
                | "stringout"
                | "lso"
                | "aao"
        )
    }

    /// Whether async processing has completed and put_notify can respond.
    /// Records that return AsyncPendingNotify should return false while
    /// async work is in progress, and true when done.
    /// Default: true (synchronous records are always complete).
    fn is_put_complete(&self) -> bool {
        true
    }

    /// Whether this record should fire its forward link after processing.
    fn should_fire_forward_link(&self) -> bool {
        true
    }

    /// Whether this record's OUT link should be written after processing.
    /// Defaults to true. Override in calcout / longout to implement OOPT
    /// conditional output (epics-base 7.0.8).
    fn should_output(&self) -> bool {
        true
    }

    /// Notify the record that the OUT-link / device write completed
    /// successfully on this cycle. The framework calls this right after
    /// the actual write so transition-detection state (e.g.
    /// `longout.pval`) can update for the next cycle's
    /// [`Self::should_output`] check. Default: no-op.
    fn on_output_complete(&mut self) {}

    /// Whether this record uses MDEL/ADEL deadband for monitor posting.
    /// Binary records (bi, bo, busy, mbbi, mbbo) return false because
    /// C EPICS always posts monitors for these record types regardless
    /// of whether the value changed.
    fn uses_monitor_deadband(&self) -> bool {
        true
    }

    /// Per-record VALUE/LOG monitor gate for record types that post a
    /// monitor *only when the value actually changed* — and have no
    /// MDEL/ADEL deadband to express that.
    ///
    /// `Some(changed)` makes the framework post the VALUE and LOG
    /// monitors iff `changed`; `None` (the default) leaves the decision
    /// to the deadband / always-post path.
    ///
    /// C `lsiRecord.c`/`lsoRecord.c` `monitor()` raise `DBE_VALUE |
    /// DBE_LOG` only when `len != olen || memcmp(oval, val, len)`. Those
    /// records return [`Self::uses_monitor_deadband`]`== false`, which
    /// otherwise routes them to the unconditional always-post path
    /// (correct for binary records, wrong for lsi/lso). Because the
    /// framework posts monitors *after* `process()` — by which point the
    /// record has already committed `oval`/`olen` — the implementation
    /// captures the comparison result during `process()` and returns the
    /// captured flag here, not a live re-comparison.
    fn monitor_value_changed(&self) -> Option<bool> {
        None
    }

    /// `menuPost` "Always" override for the VALUE / LOG monitor masks.
    ///
    /// Returns `(post_value_always, post_archive_always)`. The framework
    /// ORs these into the change-gated mask from
    /// [`Self::monitor_value_changed`], so an *unchanged* process cycle
    /// still posts `DBE_VALUE` (resp. `DBE_LOG`) when the record's MPST
    /// (resp. APST) menu field is set to `Always`.
    ///
    /// C `lsiRecord.c`/`lsoRecord.c` `monitor()` compute the VAL post
    /// mask from three independent inputs:
    ///
    /// * the change test `len != olen || memcmp(oval, val, len)` →
    ///   `DBE_VALUE | DBE_LOG`,
    /// * `if (mpst == menuPost_Always) events |= DBE_VALUE;`,
    /// * `if (apst == menuPost_Always) events |= DBE_LOG;`.
    ///
    /// [`Self::monitor_value_changed`] carries the first input; this hook
    /// carries the other two. Records without a `menuPost` field keep the
    /// default `(false, false)`, which leaves the change gate unchanged.
    fn monitor_always_post(&self) -> (bool, bool) {
        (false, false)
    }

    /// The value the MDEL/ADEL deadband is evaluated against.
    ///
    /// For most records C `monitor()` applies the value deadband to
    /// `VAL`, so the default is [`Self::val`]. A record whose monitored
    /// quantity is not its primary value must override this: the motor
    /// record, for instance, has `VAL` as the setpoint and applies
    /// MDEL/ADEL to `RBV` (the readback) — its C `monitor()` deadbands
    /// `RBV`, not `VAL`. Such a record returns its readback field here.
    ///
    /// Default is `val()`, so existing records are unaffected.
    fn monitor_deadband_value(&self) -> Option<EpicsValue> {
        self.val()
    }

    /// The FIELD whose VALUE/LOG monitor delivery the MDEL/ADEL
    /// deadband gates — the field [`Self::monitor_deadband_value`]
    /// reads. A record overriding one must override both consistently.
    ///
    /// For most records the deadband gates the primary value itself,
    /// so the default returns [`Self::primary_field`] and nothing
    /// changes. The motor record deadbands RBV: C `monitor()`
    /// (motorRecord.cc:3468-3507) throttles the RBV post with
    /// MDEL/ADEL, while VAL is posted only when an actual setpoint
    /// change marked it (M_VAL). When this returns a non-primary
    /// field, the framework's snapshot builders:
    ///
    /// * deliver THIS field on the deadband triggers (instead of raw
    ///   change-detection), and
    /// * route the primary field through generic change-detection, so
    ///   an unchanged setpoint is not re-posted on every readback
    ///   poll.
    fn monitor_deadband_field(&self) -> &'static str {
        self.primary_field()
    }

    /// Fields the record's C `monitor()` posts on every cycle whose
    /// alarm transition fired, even when their value did not change.
    ///
    /// C motorRecord.cc `monitor()` (3513-3645) computes
    /// `local_mask = monitor_mask | (MARKED(x) ? DBE_VAL_LOG : 0)`
    /// for each field in its posting list — when the alarm moved
    /// (`monitor_mask != 0`), `local_mask` is non-zero for UNMARKED
    /// fields too, so every listed field posts with `DBE_ALARM` and a
    /// `DBE_ALARM`-only subscriber observes the alarm moment on any of
    /// them. The framework's change-detection loop posts a listed,
    /// subscribed, unchanged field with the cycle's alarm bits when
    /// this list names it.
    ///
    /// Default: empty — most C record types post only their value
    /// field(s) on an alarm transition (aiRecord.c `monitor()` posts
    /// VAL with `monitor_mask` and RVAL only when it changed), which
    /// the deadband-field post already covers.
    fn alarm_cycle_monitored_fields(&self) -> &'static [&'static str] {
        &[]
    }

    /// Fields the record's C `monitor()` re-posts with `DBE_VAL_LOG` on
    /// every cycle that recomputed them, even when the value did not
    /// change — the analogue of an unconditional `MARK(field)` in C.
    ///
    /// Unlike [`Self::alarm_cycle_monitored_fields`] (which posts unchanged
    /// fields only on a cycle whose alarm transition fired), these post on
    /// any cycle the record names them, with `DBE_VALUE | DBE_LOG` (plus the
    /// cycle's alarm bits when one fired). The framework's change-detection
    /// loop posts a listed, subscribed, unchanged field with that mask.
    ///
    /// C motorRecord `process_motor_info` (motorRecord.cc:3764-3767)
    /// `MARK`s `M_DIFF`/`M_RDIF` unconditionally on every `CALLBACK_DATA`
    /// pass, and `monitor()` (3522-3531) posts them with `monitor_mask |
    /// DBE_VAL_LOG`; a `camonitor DIFF` on an axis parked at a constant
    /// non-zero following error thus gets an event every poll. The record
    /// returns the fields ONLY on the cycles it actually re-marked them (it
    /// reads its own per-cycle state), so a pass that did not recompute them
    /// does not over-post.
    ///
    /// Default: empty — most record types post a field only when it
    /// changed (or on an alarm transition), which the existing gates cover.
    fn force_posted_fields(&self) -> &'static [&'static str] {
        &[]
    }

    /// Fields the record's C `monitor()` re-posts with `DBE_LOG` ONLY on
    /// every cycle it names them, regardless of change — the analogue of
    /// an unconditional `db_post_events(field, DBE_LOG)` sweep.
    ///
    /// Distinct from [`Self::force_posted_fields`], which posts with
    /// `DBE_VALUE | DBE_LOG`: these post with `DBE_LOG` alone, so only a
    /// `DBE_LOG` (archiver) subscriber receives the unchanged-value
    /// event. The LOG sweep lands only for fields that did not change
    /// this cycle (the change/no-change branches are disjoint), so a
    /// field that also changed is not double-posted. For a field that is
    /// ALSO a [`Self::value_only_change_fields`] member the change post
    /// carries `DBE_VALUE` only, so this idle sweep is the sole source of
    /// its `DBE_LOG` events — which is exactly C's split (counting cycle
    /// → `DBE_VALUE`, idle `monitor()` → `DBE_LOG`); the scaler never
    /// changes `Sn` on an idle cycle, so the two never collide.
    ///
    /// C `scalerRecord.c` `monitor()` (scalerRecord.c:770-787) runs on
    /// every IDLE process and posts each active channel `S1..Snch` with a
    /// literal `DBE_LOG`. The scaler returns those channel field names
    /// here ONLY while idle (it reads its own `ss` state), so an archiver
    /// `camonitor SCALER:Sn` gets an event every idle scan even when the
    /// count is unchanged — while a counting cycle (which does not run C
    /// `monitor()`) returns empty.
    ///
    /// Default: empty — most record types have no LOG-only sweep.
    fn log_swept_fields(&self) -> &'static [&'static str] {
        &[]
    }

    /// Fields whose change-detected monitor post must carry `DBE_VALUE`
    /// only — the LOG bit is stripped — instead of the framework default
    /// `DBE_VALUE | DBE_LOG`.
    ///
    /// The generic change-detection post (and the deadband post for a
    /// deadband field named here) normally bundles `DBE_LOG` so an
    /// archiver subscribed `DBE_LOG` sees every value change. A record
    /// whose C `db_post_events` calls pass a literal `DBE_VALUE` for
    /// these fields names them here so the framework drops the LOG bit;
    /// the cycle's alarm bits are still OR'd in (alarm posting is a
    /// separate per-field contract, unaffected by this hook).
    ///
    /// C `scalerRecord.c` posts CNT/T/VAL/PR1/TP/FREQ and each active
    /// channel `S1..Snch` with a literal `DBE_VALUE` on a value change
    /// (scalerRecord.c:372,478,582,588 et al.); `DBE_LOG` appears ONLY in
    /// the idle `monitor()` sweep ([`Self::log_swept_fields`],
    /// scalerRecord.c:771). The two hooks are complementary: a `DBE_LOG`
    /// subscriber on `Sn` is served by the idle sweep, never by a
    /// counting-cycle value change — matching C.
    ///
    /// Default: empty — most record types post changes with
    /// `DBE_VALUE | DBE_LOG` (C `monitor_mask | DBE_VALUE | DBE_LOG`,
    /// calcRecord.c:420, subRecord.c:400).
    fn value_only_change_fields(&self) -> &'static [&'static str] {
        &[]
    }

    /// Initialize record (pass 0: field defaults; pass 1: dependent init).
    fn init_record(&mut self, _pass: u8) -> CaResult<()> {
        Ok(())
    }

    /// Post-init finalisation hook with mutable access to the
    /// framework's UDF flag. Called once after both `init_record`
    /// passes complete. Default implementation is a no-op.
    ///
    /// epics-base PR `dabcf89` (mbboDirect): when VAL is undefined
    /// at init time but the user populated B0..B1F bits, the bits
    /// should be folded into VAL and UDF cleared. The framework
    /// owns `common.udf`, so the record cannot mutate it from
    /// `init_record` alone — this hook is the controlled point of
    /// access.
    fn post_init_finalize_undef(&mut self, _udf: &mut bool) -> CaResult<()> {
        Ok(())
    }

    /// Seed the monitor/archive/alarm deadband trackers (MLST/ALST/LALM)
    /// from the initial value at iocInit, called once by the builder after
    /// both `init_record` passes and `post_init_finalize_undef`.
    ///
    /// Every C value record's `init_record` ends with
    /// `prec->mlst = prec->alst = prec->lalm = prec->val`
    /// (e.g. `longinRecord.c:120-122`, `aiRecord.c`), so the first
    /// `monitor()` evaluates `DELTA(mlst, val) > mdel` with `mlst == val`
    /// (= 0) and posts no DBE_VALUE/DBE_LOG event when the value is
    /// unchanged from its initial state. Records expose MLST/ALST/LALM as
    /// plain `f64` fields default-initialised to `0.0`; that default
    /// conflates "never published" with "published 0", so a record
    /// initialised to a *nonzero* value (constant DOL, initial VAL) used
    /// to post a spurious first-cycle update that C does not.
    ///
    /// The default seeds whichever of MLST/ALST/LALM the record actually
    /// serves from its monitor-deadband value (`val` for most records),
    /// making the invariant hold by construction for every record rather
    /// than per-type `init_record` code. It is idempotent for the record
    /// types that already seed inside `init_record`, and a no-op for
    /// records that serve none of these fields.
    fn seed_deadband_tracking(&mut self) {
        let seed = match self.monitor_deadband_value().and_then(|v| v.to_f64()) {
            Some(v) if v.is_finite() => v,
            _ => return,
        };
        for field in ["MLST", "ALST", "LALM"] {
            if self.get_field(field).is_some() {
                let _ = self.put_field(field, EpicsValue::Double(seed));
            }
        }
    }

    /// Called by the framework immediately after applying this cycle's
    /// [`Record::multi_input_links`] fetches, before `process()`.
    ///
    /// `resolved` lists the `link_field` names (the first element of
    /// each `multi_input_links` pair) whose fetch actually produced a
    /// value this cycle — i.e. the link was non-empty and the read
    /// succeeded. A link field absent from the slice either had no link
    /// configured or its DB/CA fetch failed.
    ///
    /// This is the framework analogue of C device support inspecting
    /// `RTN_SUCCESS(dbGetLink(...))` — e.g. `epidRecord.c:191-193`
    /// clears `udf` only when `dbGetLink(&prec->stpl, ...)` returns
    /// success. A record's `process()` cannot otherwise observe whether
    /// an input link's fetch succeeded, because a failed fetch simply
    /// leaves the target field unwritten.
    ///
    /// Additive, framework-set-hook pattern (same shape as
    /// [`Record::set_process_context`]). Default: ignore.
    fn set_resolved_input_links(&mut self, _resolved: &[&'static str]) {}

    /// Called before/after a field put for side-effect processing.
    fn special(&mut self, _field: &str, _after: bool) -> CaResult<()> {
        Ok(())
    }

    /// Other fields whose monitors must be posted because a put to
    /// `put_field` changed them as a side effect, without driving a full
    /// process cycle.
    ///
    /// Mirrors the explicit `db_post_events` calls a C `special()` makes:
    /// e.g. `compressRecord.c::reset` (invoked on a `SPC_RESET` write to
    /// `RES`) posts `NUSE` and `VAL` even though `RES` is not `pp(TRUE)`
    /// and so does not process. The framework posts a `VALUE|LOG` monitor
    /// for each returned field after the put. Default: none.
    fn monitor_side_effect_fields(&self, _put_field: &str) -> &'static [&'static str] {
        &[]
    }

    /// Downcast to concrete type for device support init injection.
    /// Override in record types that need device support to inject state (e.g., MotorRecord).
    fn as_any_mut(&mut self) -> Option<&mut dyn std::any::Any> {
        None
    }

    /// Whether processing this record should clear UDF.
    /// Override to return false for record types that don't produce a valid value every cycle.
    fn clears_udf(&self) -> bool {
        true
    }

    /// Whether the record's current `VAL` is undefined (UDF must
    /// stay set).
    ///
    /// C parity: `aiRecord.c:285` / `calcRecord.c::checkAlarms` /
    /// `int64inRecord.c:144` clear `UDF` **only** when the computed /
    /// read value is valid — `if (status == 0)` and, for floating
    /// records, only when `VAL` is not NaN. The framework owns
    /// `common.udf`; it calls `clears_udf()` to decide whether this
    /// record type clears UDF at all, then this method to decide
    /// whether the *value produced this cycle* is actually defined.
    ///
    /// Default: a floating `VAL` that is NaN (e.g. a calc
    /// divide-by-zero, or a soft input whose link read failed and
    /// left VAL un-updated) is undefined; everything else is defined.
    /// A record whose `val()` yields `None` (no primary value) is
    /// also treated as undefined.
    fn value_is_undefined(&self) -> bool {
        match self.val() {
            Some(EpicsValue::Double(v)) => v.is_nan(),
            Some(EpicsValue::Float(v)) => v.is_nan(),
            Some(_) => false,
            None => true,
        }
    }

    /// Per-record alarm hook — evaluate record-type-specific alarms
    /// (STATE / COS / analog limit / SOFT) and accumulate them into
    /// `nsta`/`nsev` via `recGblSetSevr`.
    ///
    /// The framework centralises the generic alarm machinery (UDF
    /// check, `recGblResetAlarms` transfer, MS/MSI/MSS link-alarm
    /// inheritance). The record-type-specific severity logic that C
    /// puts in each record's `checkAlarms()` belongs here so a record
    /// can raise its own alarms without the framework hardcoding a
    /// per-type `match` on `record_type()`.
    ///
    /// `common` is the record's [`CommonFields`]; implementations
    /// raise alarms with [`crate::server::recgbl::rec_gbl_set_sevr`]
    /// / [`crate::server::recgbl::rec_gbl_set_sevr_msg`].
    ///
    /// Default: no-op — records that have not yet migrated their
    /// `checkAlarms` logic here are still covered by the framework's
    /// legacy centralised `evaluate_alarms` match.
    fn check_alarms(&mut self, _common: &mut crate::server::record::CommonFields) {}

    /// Return multi-input link field pairs: (link_field, value_field).
    /// Override in calc, calcout, sel, sub to return INPA..INPL → A..L mappings.
    fn multi_input_links(&self) -> &[(&'static str, &'static str)] {
        &[]
    }

    /// The subset of [`Self::multi_input_links`] the framework should
    /// actually fetch this cycle, given an optional externally-resolved
    /// selector index (sel's NVL→SELN value, or `None` when no NVL link
    /// drove it). Default `None` = fetch every input link.
    ///
    /// C `selRecord.c::fetch_values` (lines 421-431) fetches ONLY INP[SELN]
    /// in `Specified` mode and all inputs otherwise; sel returns
    /// `Some(vec![INP[SELN]])` so the non-selected inputs are never read and
    /// raise no monitors or link-alarm SEVR.
    fn select_input_links(
        &self,
        _selector: Option<u16>,
    ) -> Option<Vec<(&'static str, &'static str)>> {
        None
    }

    /// Return multi-output link field pairs: (link_field, value_field).
    /// Override in transform to return OUTA..OUTP → A..P mappings.
    fn multi_output_links(&self) -> &[(&'static str, &'static str)] {
        &[]
    }

    /// Internal field write that bypasses read-only checks.
    /// Used by the framework to write values from ReadDbLink actions
    /// into fields that are normally read-only (e.g., epid.CVAL).
    /// Default implementation delegates to put_field().
    ///
    /// On the `ReadDbLink` path this is also where a pvalink NTEnum
    /// carrier ([`EpicsValue::EnumWithChoices`]) is resolved. The
    /// dbrType-blind link resolver produces it for an NTEnum source;
    /// pvxs `pvaGetValue` (`pvalink_lset.cpp:330-360`) picks
    /// label-vs-index by the TARGET field's dbrType — only a DBR_STRING
    /// target gets the `choices[index]` label, every other type takes
    /// the numeric index. Route it through [`EpicsValue::convert_to`]
    /// (the single value-coercion owner) against the target field's
    /// `db_field_type`, so the transient carrier is consumed before any
    /// record `put_field` / storage / wire path can see it. The
    /// single-INP→VAL apply path reaches the same `convert_to` via
    /// `set_val`'s `TypeMismatch` auto-coerce.
    fn put_field_internal(&mut self, name: &str, value: EpicsValue) -> CaResult<()> {
        // Input-link / internal delivery coerces the source to the target
        // field's stored type before `put_field`, mirroring C
        // `dbGetLink(DBF_<target>)`: the link layer converts any numeric
        // source to the requested type, so a record's typed `put_field`
        // arm never sees a mismatched type. This is the single owner of
        // that coercion, covering every `ReadDbLink` target by construction
        // (e.g. a `compress` INP from a `DBF_LONG` record delivers a
        // `Long`/`LongArray` that must become `Double`/`DoubleArray` for the
        // Double-only VAL arm, which otherwise drops it and never advances
        // the buffer). An `EnumWithChoices` carrier is always collapsed to a
        // bare index by `convert_to`, even when the target is already `Enum`.
        let target_type = self
            .field_list()
            .iter()
            .find(|f| f.name.eq_ignore_ascii_case(name))
            .map(|f| f.dbf_type)
            .or_else(|| self.get_field(name).map(|v| v.db_field_type()));
        let is_enum_carrier = matches!(value, EpicsValue::EnumWithChoices { .. });
        let value = match target_type {
            Some(target)
                if is_enum_carrier
                    || (value.db_field_type() != target && !value.is_empty_array()) =>
            {
                value.convert_to(target)
            }
            // Carrier with no known target field: collapse to a bare index
            // (the prior fallback) rather than letting it reach storage.
            None if is_enum_carrier => value.convert_to(DbFieldType::Long),
            _ => value,
        };
        self.put_field(name, value)
    }

    /// Return pre-process actions (ReadDbLink) that the framework should
    /// execute BEFORE calling process(). This is called once per cycle.
    /// Default returns empty. Override in records that need link reads
    /// to be available during process().
    fn pre_process_actions(&mut self) -> Vec<ProcessAction> {
        Vec::new()
    }

    /// Return actions the framework must execute BEFORE the input-link
    /// (`multi_input_links`, INP -> value-field) fetch for this cycle.
    ///
    /// This is strictly earlier than [`Self::pre_process_actions`]: the
    /// framework resolves input links *before* it calls
    /// `pre_process_actions`, so an action that must affect what an
    /// input link reads cannot be expressed there.
    ///
    /// The motivating case is the epid record's `devEpidSoftCallback`
    /// DB-type TRIG link: C `devEpidSoftCallback.c:120-132` writes the
    /// readback-trigger link with `dbPutLink` — which synchronously
    /// processes the triggered source chain — and only *then*
    /// (`devEpidSoftCallback.c:151`) does `dbGetLink(&pepid->inp, ...)`
    /// read `CVAL`. The trigger write therefore has to land before the
    /// `INP -> CVAL` fetch, in the same process pass.
    ///
    /// Called once per cycle, while a record write lock is held; the
    /// framework executes the returned actions (currently `WriteDbLink`
    /// and `ReadDbLink`) and then performs the input-link fetch.
    /// Default returns empty.
    fn pre_input_link_actions(&mut self) -> Vec<ProcessAction> {
        Vec::new()
    }

    /// Called by the framework immediately before `process()` to push a
    /// read-only snapshot of framework-owned [`CommonFields`] state
    /// ([`ProcessContext`]) that the record's `process()` needs to see.
    ///
    /// The framework owns `RecordInstance.common`; a record `process()`
    /// only gets `&mut self`. C records read `dbCommon` directly — e.g.
    /// `epidRecord.c:195` checks `pepid->udf` at the top of `process()`,
    /// `timestampRecord.c:90` branches on `ptimestamp->tse`. This hook
    /// is the controlled equivalent: a record that needs `udf`/`phas`/
    /// `tse`/`tsel` during `process()` overrides this to stash the
    /// values into its own fields.
    ///
    /// Additive, framework-set-hook pattern (same shape as
    /// [`Record::set_device_did_compute`]). Default: ignore — most
    /// records never need common state during `process()`.
    fn set_process_context(&mut self, _ctx: &ProcessContext) {}

    /// Called once by the framework when the record is registered
    /// (`add_record`), delivering the record its own canonical name plus a
    /// cycle-free [`crate::server::database::AsyncDbHandle`] for driving
    /// async-side updates from OUTSIDE a `process()` cycle.
    ///
    /// The handle wraps a `Weak` reference to the database, so a record
    /// that stashes it creates no ownership cycle (the database owns the
    /// record; a stored strong handle would leak it). It is the controlled
    /// equivalent of C device support capturing `precord` plus the
    /// dbCommon scan lock for an out-of-band `db_post_events` /
    /// `callbackRequest`: e.g. the asyn TRACE/exception callback posts
    /// trace-flag fields immediately from the driver thread, and AQR
    /// cancels a queued I/O re-entry — neither happens inside `process()`.
    ///
    /// The in-band counterpart for a record's *own* process cycle is the
    /// completion-driven [`ProcessAction`] family
    /// ([`ProcessAction::WriteDbLinkNotify`],
    /// [`ProcessAction::CancelReprocess`],
    /// [`ProcessAction::ReprocessAfter`]); this hook exists for the
    /// out-of-band path that has no `process()` return to ride on.
    ///
    /// Additive, framework-set-hook pattern (same shape as
    /// [`Self::set_process_context`]). Default: ignore — most records do
    /// no out-of-band async posting.
    fn set_async_context(&mut self, _name: String, _db: crate::server::database::AsyncDbHandle) {}

    /// Framework init hook: called once at record load *after* the common
    /// link fields (`INP`/`OUT`/`FLNK`/...) have been resolved and the
    /// `init_record` passes have run, with the record's resolved
    /// [`CommonFields`](crate::server::record::CommonFields).
    ///
    /// This is the seam for records that classify their links into status
    /// diagnostics at init the way C `init_record` does (e.g. calcout's
    /// `INAV..INUV`/`OUTV` `menu(calcoutINAV)` checkLinks loop): a record's
    /// *common* link strings (`OUT` is a common field, not a record field)
    /// are invisible to [`Self::set_async_context`] — which runs at
    /// `add_record`, *before* the common fields are applied — and to
    /// `init_record`, which carries no `CommonFields`. The record captures
    /// whichever common links it needs here so a passive, never-processed
    /// record already exposes its link status. Records whose links are all
    /// record-owned (e.g. sseq DOLn/LNKn) do not need this hook.
    ///
    /// Additive, framework-set-hook pattern. Default: ignore.
    fn init_links(&mut self, _common: &crate::server::record::CommonFields) {}

    /// Called by the framework before process() to indicate whether device
    /// support's read() already performed the record's compute step.
    /// Override in records that have a built-in compute (e.g., epid PID)
    /// to skip it when device support already ran it.
    /// Default: ignore.
    fn set_device_did_compute(&mut self, _did_compute: bool) {}

    /// Whether this record has a raw-to-engineering (`RVAL → VAL`)
    /// `convert()` step that must be skipped on a `Soft Channel` input.
    ///
    /// C `devAiSoft.c:65` `read_ai` (and the other soft-channel input
    /// `read_xxx`) always returns 2 ("don't convert"), so `aiRecord.c`'s
    /// `if (status==0) convert(prec)` is bypassed for a `Soft Channel`
    /// input record. The framework expresses this by calling
    /// [`Record::set_device_did_compute(true)`] on the record before
    /// `process()`.
    ///
    /// This hook exists so the framework only suppresses `convert()` —
    /// NOT a record's entire built-in compute. Records like `epid` also
    /// override `set_device_did_compute` but interpret it as "skip the
    /// whole compute step" (the PID loop); those records have no
    /// `RVAL → VAL` convert and MUST keep the default `false` so a
    /// `Soft Channel` `epid` still runs `do_pid()` in `process()`.
    ///
    /// Default `false`: a record is only opted into the soft-channel
    /// convert-skip when it explicitly returns `true`.
    fn soft_channel_skips_convert(&self) -> bool {
        false
    }
}

/// Subroutine function type for sub records.
pub type SubroutineFn = Box<dyn Fn(&mut dyn Record) -> CaResult<()> + Send + Sync>;
