use crate::error::{CaError, CaResult};
use crate::server::record::{
    FieldMetadataOverride, MENU_SIMM, ProcessOutcome, RawSoftEntry, Record, ValuePostGate,
};
use crate::server::records::convert_phase::ConvertPhase;
use crate::types::{EpicsValue, PvString};

/// The soft `ai` dset's own "a read has completed" state — `prec->dpvt` in
/// `devAiSoft.c` (set at :90, cleared at :94) and `pdevPvt->smooth` in
/// `devAiSoftCallback.c` (:194, :182). It is the second term of the three-part
/// `SMOO` guard, and it is what makes the FIRST soft read of a record — and
/// the first after a failed one — land unblended.
///
/// Named rather than a bare `bool` because the state it carries is not "SMOO
/// is armed" but "`VAL` currently holds a previous reading": `VAL` alone
/// cannot say so (a caput, a `SIMM` landing and a constant-`INP` load all
/// leave a perfectly finite `VAL` that C refuses to blend against).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum SoftReadHistory {
    /// C `dpvt == NULL` / `smooth == FALSE`.
    #[default]
    Absent,
    /// C `dpvt != NULL` / `smooth == TRUE`.
    Prior,
}

/// Analog input record with conversion support.
/// LINR: 0=NO_CONVERSION, 1=SLOPE, 2=LINEAR
pub struct AiRecord {
    // Display
    pub val: f64,
    pub egu: PvString,
    pub hopr: f64,
    pub lopr: f64,
    pub prec: i16,
    // Conversion
    pub rval: i32,
    pub oraw: i32, // old raw value for monitor change detection
    pub linr: i16, // 0=NO_CONVERSION, 1=SLOPE, 2=LINEAR
    pub eguf: f64,
    pub egul: f64,
    pub eslo: f64, // default 1.0
    pub eoff: f64, // engineering offset (defaults to egul for LINEAR)
    // ROFF is DBF_ULONG (aiRecord.dbd.pod:442) — stored unsigned so the full
    // 0..=2^32-1 range round-trips: a client caput 4294967295 reads back as
    // that value (promoted to DBR_DOUBLE over CA), and the RVAL+ROFF convert
    // uses C's epicsUInt32 addend. Storing it signed made a high-bit put read
    // back negative and convert with the wrong sign.
    pub roff: u32,
    pub aslo: f64, // default 1.0
    pub aoff: f64,
    pub smoo: f64, // smoothing 0~1
    // Deadband
    pub adel: f64,
    pub mdel: f64,
    // Alarm-range time-constant filter (aiRecord.c::checkAlarms:355-401).
    // AFTC > 0 enables an exponential smoothing of the integer alarmRange
    // (1=Lolo..5=Hihi) so transient excursions don't immediately alarm.
    // AFVL is the filter accumulator (sign encodes rounding hysteresis).
    pub aftc: f64,
    pub afvl: f64,
    // Runtime (alarm/monitor tracking)
    pub lalm: f64,
    pub alst: f64,
    pub mlst: f64,
    /// C `prec->init` — see [`ConvertPhase`]. Set by `init_record` and by an
    /// `SPC_LINCONV` put, cleared at the end of every `process`.
    pub init: ConvertPhase,
    skip_convert: bool,
    /// C's soft-dset history flag — see [`SoftReadHistory`]. Owned by
    /// [`Record::soft_input_read`], which is the only place C touches
    /// `dpvt`/`smooth` too.
    soft_history: SoftReadHistory,
    /// Set by `process()` when a `LINR >= 3` breakpoint-table conversion
    /// fails — the table could not be resolved, or the raw value fell past
    /// an end of the table. C `aiRecord.c::convert` raises
    /// `SOFT_ALARM/MAJOR_ALARM` ("BPT Error") in both cases
    /// (aiRecord.c:434-435); `check_alarms` consumes this flag to do the
    /// same so the misconfiguration / out-of-range is not silent.
    bpt_error: bool,
    /// The database breakpoint-table registry (`install_breaktable_registry`),
    /// used to resolve the `LINR >= 3` table lazily.
    bpt_registry: Option<std::sync::Arc<crate::server::cvt_bpt::BreakTableRegistry>>,
    /// The resolved breakpoint table for the current `LINR` (C `pbrk` cache).
    /// `None` until the first `LINR >= 3` conversion resolves it; reset to
    /// `None` when `LINR` changes so the next conversion re-resolves.
    bpt_table: Option<std::sync::Arc<crate::server::cvt_bpt::BrkTable>>,
    /// Cached last breakpoint interval index (C `lbrk`), so a monotonic input
    /// does not re-scan the table from the start each conversion.
    lbrk: usize,
    // Simulation
    pub simm: i16,
    pub siml: String,
    pub siol: String,
    pub sims: i16,
    pub sdly: f64,
    /// Simulation value (`field(SVAL,DBF_DOUBLE)`, aiRecord.dbd). The value
    /// used when `SIMM=YES` and `SIOL` is unset; also repurposed by asyn's
    /// averaging device support as the I/O-Intr decimation count
    /// (devAsynInt32.c:667). Settable, no special/menu, C default 0.0.
    pub sval: f64,
}

impl Default for AiRecord {
    fn default() -> Self {
        Self {
            val: 0.0,
            egu: PvString::new(),
            hopr: 0.0,
            lopr: 0.0,
            prec: 0,
            rval: 0,
            oraw: 0,
            linr: 0,
            eguf: 0.0,
            egul: 0.0,
            eslo: 1.0,
            eoff: 0.0,
            roff: 0,
            aslo: 1.0,
            aoff: 0.0,
            smoo: 0.0,
            adel: 0.0,
            mdel: 0.0,
            aftc: 0.0,
            afvl: 0.0,
            lalm: 0.0,
            alst: 0.0,
            mlst: 0.0,
            init: ConvertPhase::default(),
            skip_convert: false,
            soft_history: SoftReadHistory::default(),
            bpt_error: false,
            bpt_registry: None,
            bpt_table: None,
            lbrk: 0,
            simm: 0,
            siml: String::new(),
            siol: String::new(),
            sims: 0,
            sdly: -1.0,
            sval: 0.0,
        }
    }
}

impl AiRecord {
    pub fn new(val: f64) -> Self {
        Self {
            val,
            ..Default::default()
        }
    }
}

impl Record for AiRecord {
    fn record_type(&self) -> &'static str {
        "ai"
    }

    /// `aiRecord.c:267-288` `get_control_double` lists one field the shared
    /// VAL-class set does not: `SVAL`, which takes the record's own
    /// `HOPR`/`LOPR` like `VAL` does. Without this it falls to the `default:`
    /// arm and reports the DBF_DOUBLE range of ±1e300.
    fn field_metadata_override(&self, field: &str) -> Option<FieldMetadataOverride> {
        field
            .eq_ignore_ascii_case("SVAL")
            .then(|| FieldMetadataOverride {
                ctrl_limits: Some((self.hopr, self.lopr)),
                ..Default::default()
            })
    }

    fn init_record(&mut self, pass: u8) -> CaResult<()> {
        if pass == 0 {
            // Legacy compatibility: if eslo==1.0 && eoff==0.0, set eoff from egul.
            // Save eoff/eslo first in case SLOPE mode needs to preserve them.
            let saved_eoff = self.eoff;
            let saved_eslo = self.eslo;

            if self.eslo == 1.0 && self.eoff == 0.0 {
                self.eoff = self.egul;
            }

            // For SLOPE mode, restore user-configured eoff/eslo
            if self.linr == 1 {
                self.eoff = saved_eoff;
                self.eslo = saved_eslo;
            }

            // Initialize tracking fields from current val
            self.mlst = self.val;
            self.alst = self.val;
            self.lalm = self.val;
            self.oraw = self.rval;

            // C `aiRecord.c:114`: `prec->init = TRUE;` — an initialised record
            // has not converted yet, so the next conversion is the initial one.
            self.init = ConvertPhase::Initial;
        }
        Ok(())
    }

    fn process(&mut self) -> CaResult<ProcessOutcome> {
        if !self.skip_convert {
            // convert() - raw to engineering units conversion
            // Step 1: Apply ROFF, then ASLO/AOFF (always, regardless of LINR)
            let mut v = (self.rval as f64) + (self.roff as f64);
            if self.aslo != 0.0 {
                v *= self.aslo;
            }
            v += self.aoff;

            // Step 2: Apply linearization based on LINR
            self.bpt_error = false;
            match self.linr {
                0 => {} // NO_CONVERSION: skip linearization
                1 | 2 => {
                    // SLOPE (1) and LINEAR (2): apply eslo/eoff
                    v = v * self.eslo + self.eoff;
                }
                _ => {
                    // LINR >= 3 selects a breakpoint-table linearisation.
                    // Resolve the table lazily from the registry and cache it
                    // (C `cvtRawToEngBpt` resolves via `findBrkTable` on the
                    // first call / after a LINR change, then caches `pbrk`).
                    if self.bpt_table.is_none() {
                        self.bpt_table = self
                            .bpt_registry
                            .as_ref()
                            .and_then(|reg| reg.table_for_linr(self.linr));
                    }
                    match &self.bpt_table {
                        Some(table) => {
                            // C applies the (possibly extrapolated) value even
                            // out of range, but still raises the alarm
                            // (aiRecord.c:434-435 does not early-return).
                            let (eng, status) = crate::server::cvt_bpt::cvt_raw_to_eng_bpt(
                                v,
                                table,
                                &mut self.lbrk,
                            );
                            v = eng;
                            if status == crate::server::cvt_bpt::BptStatus::OutOfRange {
                                self.bpt_error = true;
                            }
                        }
                        None => {
                            // Table not found: C `cvtRawToEngBpt` returns
                            // `S_dbLib_badField` before touching `*pval`, so the
                            // value is left at the pre-linearisation `v`; the
                            // record raises `SOFT_ALARM/MAJOR_ALARM`.
                            self.bpt_error = true;
                        }
                    }
                }
            }

            // Step 3: Smoothing filter (aiRecord.c:440-444). On the initial
            // conversion there is no history to blend against, so C seeds VAL
            // with the new value first — `if (prec->init) prec->val = val;` —
            // and only then runs the filter.
            if self.smoo != 0.0 && self.val.is_finite() {
                if self.init.is_initial() {
                    self.val = v;
                }
                self.val = v * (1.0 - self.smoo) + self.val * self.smoo;
            } else {
                self.val = v;
            }
        } // end skip_convert
        self.skip_convert = false;

        self.oraw = self.rval;
        // C `aiRecord.c:170`: `prec->init = FALSE;` on the way out of EVERY
        // process, not just the first one.
        self.init = ConvertPhase::Converted;
        Ok(ProcessOutcome::complete())
    }

    fn get_field(&self, name: &str) -> Option<EpicsValue> {
        match name {
            "VAL" => Some(EpicsValue::Double(self.val)),
            "EGU" => Some(EpicsValue::String(self.egu.clone())),
            "HOPR" => Some(EpicsValue::Double(self.hopr)),
            "LOPR" => Some(EpicsValue::Double(self.lopr)),
            "PREC" => Some(EpicsValue::Short(self.prec)),
            "RVAL" => Some(EpicsValue::Long(self.rval)),
            "ORAW" => Some(EpicsValue::Long(self.oraw)),
            "LINR" => Some(EpicsValue::Short(self.linr)),
            "EGUF" => Some(EpicsValue::Double(self.eguf)),
            "EGUL" => Some(EpicsValue::Double(self.egul)),
            "ESLO" => Some(EpicsValue::Double(self.eslo)),
            "EOFF" => Some(EpicsValue::Double(self.eoff)),
            "ROFF" => Some(EpicsValue::ULong(self.roff)),
            "ASLO" => Some(EpicsValue::Double(self.aslo)),
            "AOFF" => Some(EpicsValue::Double(self.aoff)),
            "SMOO" => Some(EpicsValue::Double(self.smoo)),
            "ADEL" => Some(EpicsValue::Double(self.adel)),
            "MDEL" => Some(EpicsValue::Double(self.mdel)),
            "AFTC" => Some(EpicsValue::Double(self.aftc)),
            "AFVL" => Some(EpicsValue::Double(self.afvl)),
            "LALM" => Some(EpicsValue::Double(self.lalm)),
            "ALST" => Some(EpicsValue::Double(self.alst)),
            "MLST" => Some(EpicsValue::Double(self.mlst)),
            "INIT" => Some(self.init.as_field()),
            "SIMM" => Some(EpicsValue::Short(self.simm)),
            "SIML" => Some(EpicsValue::String(self.siml.clone().into())),
            "SIOL" => Some(EpicsValue::String(self.siol.clone().into())),
            "SIMS" => Some(EpicsValue::Short(self.sims)),
            "SDLY" => Some(EpicsValue::Double(self.sdly)),
            "SVAL" => Some(EpicsValue::Double(self.sval)),
            _ => None,
        }
    }

    fn put_field(&mut self, name: &str, value: EpicsValue) -> CaResult<()> {
        match name {
            "VAL" => match value {
                EpicsValue::Double(v) => {
                    self.val = v;
                    Ok(())
                }
                EpicsValue::Long(v) => {
                    self.val = v as f64;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch(name.into())),
            },
            "EGU" => match value {
                EpicsValue::String(v) => {
                    self.egu = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch(name.into())),
            },
            "HOPR" => match value {
                EpicsValue::Double(v) => {
                    self.hopr = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch(name.into())),
            },
            "LOPR" => match value {
                EpicsValue::Double(v) => {
                    self.lopr = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch(name.into())),
            },
            "PREC" => match value {
                EpicsValue::Short(v) => {
                    self.prec = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch(name.into())),
            },
            "RVAL" => match value {
                EpicsValue::Long(v) => {
                    self.rval = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch(name.into())),
            },
            // SPC_LINCONV parity (aiRecord.c:182-200): LINR / EGUF / EGUL
            // are tagged `special(SPC_LINCONV)` in aiRecord.dbd, and C's
            // `special()` does exactly two things for them:
            //
            // ```c
            // prec->init = TRUE;
            // if ((prec->linr == menuConvertLINEAR) && pdset->special_linconv) {
            //     prec->eoff = prec->egul;              /* <- INSIDE the gate */
            //     status = (*pdset->special_linconv)(prec, after);
            //     ...
            // }
            // return 0;
            // ```
            //
            // The `eoff = egul` rebase belongs to the device support's
            // `special_linconv` callback, and NO soft dset provides one:
            // `devAiSoftRaw.c:32-35` is `{{6, NULL, NULL, init_record, NULL},
            // read_ai, NULL}` — that trailing NULL *is* `special_linconv`. So
            // on a soft record a put to LINR/EGUF/EGUL sets `init` and touches
            // nothing else, and an operator retuning the display range EGUL
            // does not silently re-scale the conversion.
            //
            // The arm C opens with — `if (pdset->common.number < 6) {
            // recGblDbaddrError(S_db_noMod, paddr, "ai: special"); return
            // S_db_noMod; }` (aiRecord.c:183-186), refusing a legacy 5-entry
            // DSET table that has no `special_linconv` slot — is deliberately
            // absent here: every base-supplied soft dset declares `{6, ...}`
            // (devAiSoft.c:34, devAiSoftRaw.c:33) so the refusal is
            // unreachable through base's own device support, and this port's
            // device support is the `DeviceSupport` trait
            // (`server/device_support.rs:96`), which has no `number` field and
            // no function-pointer table, so a short dset is unrepresentable by
            // type rather than rejected at run time.
            //
            "LINR" => match value {
                EpicsValue::Short(v) => {
                    self.linr = v;
                    self.init = ConvertPhase::Initial;
                    // A LINR change re-selects the breakpoint table (C
                    // `special_linconv` sets `init=TRUE`, which makes the next
                    // `cvtRawToEngBpt` re-resolve via `findBrkTable`). Drop the
                    // cached table + interval index so the next conversion
                    // resolves the new LINR.
                    self.bpt_table = None;
                    self.lbrk = 0;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch(name.into())),
            },
            "EGUF" => match value {
                EpicsValue::Double(v) => {
                    self.eguf = v;
                    self.init = ConvertPhase::Initial;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch(name.into())),
            },
            "EGUL" => match value {
                EpicsValue::Double(v) => {
                    self.egul = v;
                    self.init = ConvertPhase::Initial;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch(name.into())),
            },
            "ESLO" => match value {
                EpicsValue::Double(v) => {
                    self.eslo = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch(name.into())),
            },
            "EOFF" => match value {
                EpicsValue::Double(v) => {
                    self.eoff = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch(name.into())),
            },
            // DBF_ULONG: accept the native unsigned carrier and the legacy signed
            // Long (bit-preserving), so a full-range caput (4294967295) stores
            // rather than being rejected as a type mismatch.
            "ROFF" => match value {
                EpicsValue::ULong(v) => {
                    self.roff = v;
                    Ok(())
                }
                EpicsValue::Long(v) => {
                    self.roff = v as u32;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch(name.into())),
            },
            "ASLO" => match value {
                EpicsValue::Double(v) => {
                    self.aslo = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch(name.into())),
            },
            "AOFF" => match value {
                EpicsValue::Double(v) => {
                    self.aoff = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch(name.into())),
            },
            "SMOO" => match value {
                EpicsValue::Double(v) => {
                    self.smoo = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch(name.into())),
            },
            "ADEL" => match value {
                EpicsValue::Double(v) => {
                    self.adel = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch(name.into())),
            },
            "MDEL" => match value {
                EpicsValue::Double(v) => {
                    self.mdel = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch(name.into())),
            },
            "AFTC" => match value {
                EpicsValue::Double(v) => {
                    self.aftc = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch(name.into())),
            },
            // AFVL is SPC_NOMOD (read-only to clients); this arm exists so
            // the framework alarm-filter owner can write the accumulator back.
            "AFVL" => match value {
                EpicsValue::Double(v) => {
                    self.afvl = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch(name.into())),
            },
            // Runtime fields (writable internally)
            "LALM" => match value {
                EpicsValue::Double(v) => {
                    self.lalm = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch(name.into())),
            },
            "ALST" => match value {
                EpicsValue::Double(v) => {
                    self.alst = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch(name.into())),
            },
            "MLST" => match value {
                EpicsValue::Double(v) => {
                    self.mlst = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch(name.into())),
            },
            "SIMM" => match value {
                EpicsValue::Short(v) => {
                    self.simm = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch(name.into())),
            },
            "SIML" => match value {
                EpicsValue::String(v) => {
                    self.siml = v.as_str_lossy().into_owned();
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch(name.into())),
            },
            "SIOL" => match value {
                EpicsValue::String(v) => {
                    self.siol = v.as_str_lossy().into_owned();
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch(name.into())),
            },
            "SIMS" => match value {
                EpicsValue::Short(v) => {
                    self.sims = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch(name.into())),
            },
            "SDLY" => match value {
                EpicsValue::Double(v) => {
                    self.sdly = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch(name.into())),
            },
            "SVAL" => match value {
                EpicsValue::Double(v) => {
                    self.sval = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch(name.into())),
            },
            _ => Err(CaError::FieldNotFound(name.into())),
        }
    }

    /// `SIMM` is `DBF_MENU menu(menuSimm)` (`aiRecord.dbd.pod`): the analog
    /// records carry the three-choice NO/YES/RAW simulation menu, not the
    /// two-choice `menuYesNo` used by the integer/string records. Served as
    /// `DBR_ENUM` with these labels. `SIMS`/`OLDSIMM` are shared menus
    /// resolved centrally.
    fn menu_field_choices(&self, field: &str) -> Option<&'static [&'static str]> {
        match field {
            "SIMM" => Some(MENU_SIMM),
            _ => None,
        }
    }

    /// C `aiRecord.c::convert` raises `SOFT_ALARM/MAJOR_ALARM` when the
    /// breakpoint-table conversion fails. epics-base-rs has no
    /// breakpoint-table registry, so any `LINR >= 3` is an unresolvable
    /// table — `process()` flags `bpt_error` and this hook raises the
    /// C-equivalent alarm so the misconfiguration is visible.
    fn check_alarms(&mut self, common: &mut crate::server::record::CommonFields) {
        if self.bpt_error {
            crate::server::recgbl::rec_gbl_set_sevr_msg(
                common,
                crate::server::recgbl::alarm_status::SOFT_ALARM,
                crate::server::record::AlarmSeverity::Major,
                "BPT Error",
            );
        }
    }

    fn set_device_did_compute(&mut self, did_compute: bool) {
        self.skip_convert = did_compute;
    }

    fn install_breaktable_registry(
        &mut self,
        registry: std::sync::Arc<crate::server::cvt_bpt::BreakTableRegistry>,
    ) {
        self.bpt_registry = Some(registry);
        // Force a re-resolve on the next conversion in case LINR was already
        // set to a breakpoint table before the registry was installed.
        self.bpt_table = None;
    }

    /// `ai` has an `RVAL → VAL` `convert()` step (raw → engineering
    /// units). A `Soft Channel` `ai` must skip it — C `devAiSoft.c`
    /// `read_ai` returns 2 ("don't convert").
    fn soft_channel_skips_convert(&self) -> bool {
        true
    }

    /// C `devAiSoft.c::read_ai` (:81-93) and `devAiSoftCallback.c::read_ai`
    /// (:180-194): both soft `ai` dsets apply `SMOO` themselves, before the
    /// record body ever runs. `aiRecord.c:440-444` — the copy `process()`
    /// runs — is reached only when `convert()` does, i.e. only under
    /// `devAiSoftRaw`, so with the filter written there alone `SMOO` had no
    /// effect at any value on the default `ai` DTYP.
    ///
    /// C's guard is three-part: `smoo != 0.0 && prec->dpvt && finite(prec->val)`,
    /// and at that point `prec->val` is still the PREVIOUS value — the reading
    /// is `vt.val`. So the finiteness test is on what is being blended INTO,
    /// and `dpvt` is the separate answer to "is there anything to blend into".
    fn soft_input_read(&mut self, value: Option<EpicsValue>) -> CaResult<()> {
        let Some(value) = value else {
            self.soft_history = SoftReadHistory::Absent;
            return Ok(());
        };
        let previous = self.val;
        let history = self.soft_history;
        // `set_val` stays the coercion owner (an array source delivers
        // `wf[0]`, C `dbGetLink` with `nRequest = NULL`), so the reading is
        // taken through it and blended afterwards.
        self.set_val(value)?;
        if self.smoo != 0.0 && history == SoftReadHistory::Prior && previous.is_finite() {
            self.val = self.val * (1.0 - self.smoo) + previous * self.smoo;
        }
        self.soft_history = SoftReadHistory::Prior;
        Ok(())
    }

    /// C `devAiSoftRaw` — `recGblInitConstantLink(&prec->inp, DBF_LONG,
    /// &prec->rval)` at init (`devAiSoftRaw.c:38-42`), `dbGetLink(pinp,
    /// DBR_LONG, &prec->rval, 0, 0)` per read (`:50`), and `read_ai` returns 0,
    /// so the record's own `RVAL → VAL` convert (ROFF/ASLO/AOFF/LINR/SMOO) runs.
    /// `ai` has no MASK, so both entry points store the same word.
    fn raw_soft_input(&mut self, _entry: RawSoftEntry, value: EpicsValue) -> Option<CaResult<()>> {
        // RVAL is epicsInt32 and the link is read as DBR_LONG; the conversion is
        // owned by `raw_soft_rval_i32`, not by a bare `c_cast` of `to_f64()`.
        self.rval = match super::raw_soft_rval_i32("ai", &value) {
            Ok(rval) => rval,
            Err(e) => return Some(Err(e)),
        };
        Some(Ok(()))
    }

    /// C `aiRecord.c:460-465` posts `RVAL` with VAL's own `monitor_mask`,
    /// nested in `if (monitor_mask)`: RVAL is posted only when VAL is
    /// posted this cycle (alarm change or MDEL/ADEL crossing) and RVAL
    /// changed — never with a forced `DBE_VALUE | DBE_LOG`. Under a
    /// non-default MDEL the raw count can change while VAL stays inside the
    /// deadband, so RVAL must NOT post on the default aux path.
    fn fields_posted_with_value_mask(&self) -> &'static [(&'static str, ValuePostGate)] {
        // C re-tests the raw value inside the guard (`if (prec->oraw !=
        // prec->rval)`, aiRecord.c:462), so an unchanged RVAL is not re-posted.
        &[("RVAL", ValuePostGate::OnChange)]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// ROFF is DBF_ULONG (aiRecord.dbd.pod:442): a client caput of the full
    /// unsigned range stores and reads back as that value (served ULong,
    /// promoted to DBR_DOUBLE over CA). The native ULong carrier and the legacy
    /// signed Long (bit-preserving) are both accepted; storing was previously
    /// signed and rejected the ULong carrier a full-range caput arrives as.
    #[test]
    fn roff_put_accepts_full_ulong_range() {
        let mut rec = AiRecord::default();
        rec.put_field("ROFF", EpicsValue::ULong(u32::MAX)).unwrap();
        assert_eq!(rec.get_field("ROFF"), Some(EpicsValue::ULong(u32::MAX)));
        // Legacy signed carrier reinterprets bit-for-bit.
        rec.put_field("ROFF", EpicsValue::Long(-1)).unwrap();
        assert_eq!(rec.get_field("ROFF"), Some(EpicsValue::ULong(u32::MAX)));
    }

    /// SPC_LINCONV parity (aiRecord.c:187): writing LINR / EGUF / EGUL re-arms
    /// the INIT phase, so the next process takes the retuned conversion as its
    /// initial condition instead of blending it into the SMOO history. This
    /// test previously asserted `!rec.init` — the port's inverted "primed" bit —
    /// and so pinned the polarity C serves on the wire (`INIT = 1` while
    /// initial). The behaviour it checks is unchanged; the flag it reads is the
    /// C one. See tests/init_phase.rs for the whole phase.
    #[test]
    fn linr_put_re_arms_the_init_phase_for_smoothing() {
        let mut rec = AiRecord::default();
        rec.init = ConvertPhase::Converted; // post-first-process
        rec.smoo = 0.5;

        rec.put_field("LINR", EpicsValue::Short(2)).unwrap();

        assert!(
            rec.init.is_initial(),
            "LINR put re-arms INIT so the next process reprimes SMOO"
        );
    }

    /// SPC_LINCONV parity (aiRecord.c:188-198). This test previously asserted
    /// the OPPOSITE — that an EGUL put under LINEAR rebases `eoff = egul` — and
    /// so pinned the defect (R18-97). C's `prec->eoff = prec->egul;` sits INSIDE
    /// `if ((prec->linr == menuConvertLINEAR) && pdset->special_linconv)`: the
    /// rebase is the device support's, and `devAiSoftRaw.c:32-35` supplies no
    /// `special_linconv` (the trailing NULL of `{{6, NULL, NULL, init_record,
    /// NULL}, read_ai, NULL}`).
    ///
    /// Oracle (softIoc 7.0.10.1-DEV, `DTYP="Raw Soft Channel"`, `INP="12"`,
    /// LINR=LINEAR, EOFF=0): `dbpf T:AI.EGUL 7.25` → EOFF stays **0** and VAL
    /// stays **12**. Pre-fix the port gave EOFF=7.25 and VAL=19.25 — an
    /// operator retuning the display range silently re-scaled the reading.
    #[test]
    fn egul_put_under_linear_does_not_rebase_eoff() {
        let mut rec = AiRecord::default();
        rec.linr = 2; // LINEAR
        rec.eoff = 1.5;

        rec.put_field("EGUL", EpicsValue::Double(7.25)).unwrap();

        assert_eq!(
            rec.eoff, 1.5,
            "no soft dset provides special_linconv, so C leaves EOFF alone"
        );
        assert_eq!(rec.egul, 7.25, "the display range itself is written");
    }

    /// The other two `special(SPC_LINCONV)` fields take the same gate — it is
    /// the dset that decides, not which field was written.
    #[test]
    fn linr_and_eguf_puts_do_not_rebase_eoff() {
        let mut rec = AiRecord::default();
        rec.eoff = 1.5;
        rec.egul = 7.25;

        rec.put_field("LINR", EpicsValue::Short(2)).unwrap();
        assert_eq!(rec.eoff, 1.5, "LINR put: EOFF untouched");

        rec.put_field("EGUF", EpicsValue::Double(100.0)).unwrap();
        assert_eq!(rec.eoff, 1.5, "EGUF put: EOFF untouched");
    }

    /// The consequence the oracle measured: the raw→engineering conversion is
    /// unchanged by an EGUL retune, so VAL does not move.
    #[test]
    fn egul_retune_does_not_move_val_under_linear() {
        let mut rec = AiRecord::default();
        rec.linr = 2; // LINEAR
        rec.eslo = 1.0;
        rec.eoff = 0.0;
        rec.put_field("RVAL", EpicsValue::Long(12)).unwrap();
        rec.process().unwrap();
        assert_eq!(rec.val, 12.0, "RVAL 12 -> VAL 12 (eslo=1, eoff=0)");

        rec.put_field("EGUL", EpicsValue::Double(7.25)).unwrap();
        rec.process().unwrap();
        assert_eq!(
            rec.val, 12.0,
            "the oracle's T:AI.VAL stays 12 after `dbpf T:AI.EGUL 7.25`; \
             pre-fix the port reported 19.25"
        );
    }

    /// SLOPE (LINR=1) preserves user-configured eoff/eslo across EGUL
    /// edits — the C path only rebases eoff when LINR == menuConvertLINEAR.
    #[test]
    fn egul_put_under_slope_mode_does_not_touch_eoff() {
        let mut rec = AiRecord::default();
        rec.linr = 1; // SLOPE
        rec.eoff = 1.5;

        rec.put_field("EGUL", EpicsValue::Double(7.25)).unwrap();

        assert_eq!(
            rec.eoff, 1.5,
            "EGUL put under SLOPE must leave user-configured eoff alone"
        );
    }

    /// A monotonic-up ramp table: raw 0..300 -> eng 0..30, slope 0.1.
    fn ramp_registry() -> std::sync::Arc<crate::server::cvt_bpt::BreakTableRegistry> {
        let mut reg = crate::server::cvt_bpt::BreakTableRegistry::new();
        reg.insert(
            crate::server::cvt_bpt::BrkTable::build(
                "ramp",
                &[(0.0, 0.0), (100.0, 10.0), (300.0, 30.0)],
            )
            .unwrap(),
        );
        std::sync::Arc::new(reg)
    }

    /// LINR >= 3 resolves the named breakpoint table from the installed
    /// registry and converts raw -> eng in range (C `cvtRawToEngBpt`).
    #[test]
    fn linr_breaktable_converts_raw_to_eng_in_range() {
        let mut rec = AiRecord::default();
        rec.install_breaktable_registry(ramp_registry());
        rec.aslo = 1.0;
        rec.aoff = 0.0;
        rec.roff = 0;
        // "ramp" is the only registered table -> first user-table index (15).
        rec.put_field(
            "LINR",
            EpicsValue::Short(crate::server::cvt_bpt::LINR_FIRST_USER_TABLE),
        )
        .unwrap();
        rec.put_field("RVAL", EpicsValue::Long(50)).unwrap();

        rec.process().unwrap();

        // raw 50 in [0,100] -> eng = 0 + (50-0)*0.1 = 5.0.
        assert_eq!(rec.get_field("VAL"), Some(EpicsValue::Double(5.0)));
        assert!(
            !rec.bpt_error,
            "in-range conversion must not raise BPT error"
        );
    }

    /// Out of range: ai still applies the extrapolated value (C does NOT
    /// early-return, aiRecord.c:433-436) but raises SOFT_ALARM/MAJOR via
    /// `bpt_error`.
    #[test]
    fn linr_breaktable_out_of_range_extrapolates_and_flags() {
        let mut rec = AiRecord::default();
        rec.install_breaktable_registry(ramp_registry());
        rec.aslo = 1.0;
        rec.put_field(
            "LINR",
            EpicsValue::Short(crate::server::cvt_bpt::LINR_FIRST_USER_TABLE),
        )
        .unwrap();
        rec.put_field("RVAL", EpicsValue::Long(400)).unwrap();

        rec.process().unwrap();

        // raw 400 past the high end (300): eng = 30 + (400-300)*0.1 = 40.0.
        assert_eq!(rec.get_field("VAL"), Some(EpicsValue::Double(40.0)));
        assert!(
            rec.bpt_error,
            "out-of-range conversion must raise BPT error"
        );
    }

    /// A LINR that selects no registered table flags the error and leaves the
    /// value at the pre-linearisation raw (C `cvtRawToEngBpt` returns before
    /// touching `*pval`).
    #[test]
    fn linr_breaktable_not_found_flags_and_passes_raw_through() {
        let mut rec = AiRecord::default();
        rec.install_breaktable_registry(ramp_registry());
        rec.aslo = 1.0;
        rec.aoff = 0.0;
        rec.roff = 0;
        // "ramp" is at index 15; this user-block index has no loaded table.
        rec.put_field(
            "LINR",
            EpicsValue::Short(crate::server::cvt_bpt::LINR_FIRST_USER_TABLE + 1),
        )
        .unwrap();
        rec.put_field("RVAL", EpicsValue::Long(50)).unwrap();

        rec.process().unwrap();

        // Value left at the raw (no linearisation applied), error flagged.
        assert_eq!(rec.get_field("VAL"), Some(EpicsValue::Double(50.0)));
        assert!(rec.bpt_error, "unresolved table must raise BPT error");
    }
}
