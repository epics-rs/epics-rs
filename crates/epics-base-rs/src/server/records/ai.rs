use crate::error::{CaError, CaResult};
use crate::server::record::{FieldDesc, MENU_SIMM, ProcessOutcome, Record};
use crate::types::{DbFieldType, EpicsValue, PvString};

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
    pub roff: i32,
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
    pub init: bool,
    skip_convert: bool,
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
            init: false,
            skip_convert: false,
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

static FIELDS: &[FieldDesc] = &[
    FieldDesc {
        name: "VAL",
        dbf_type: DbFieldType::Double,
        read_only: false,
    },
    FieldDesc {
        name: "EGU",
        dbf_type: DbFieldType::String,
        read_only: false,
    },
    FieldDesc {
        name: "HOPR",
        dbf_type: DbFieldType::Double,
        read_only: false,
    },
    FieldDesc {
        name: "LOPR",
        dbf_type: DbFieldType::Double,
        read_only: false,
    },
    FieldDesc {
        name: "PREC",
        dbf_type: DbFieldType::Short,
        read_only: false,
    },
    FieldDesc {
        name: "RVAL",
        dbf_type: DbFieldType::Long,
        read_only: false,
    },
    FieldDesc {
        name: "ORAW",
        dbf_type: DbFieldType::Long,
        read_only: true,
    },
    FieldDesc {
        name: "LINR",
        dbf_type: DbFieldType::Short,
        read_only: false,
    },
    FieldDesc {
        name: "EGUF",
        dbf_type: DbFieldType::Double,
        read_only: false,
    },
    FieldDesc {
        name: "EGUL",
        dbf_type: DbFieldType::Double,
        read_only: false,
    },
    FieldDesc {
        name: "ESLO",
        dbf_type: DbFieldType::Double,
        read_only: false,
    },
    FieldDesc {
        name: "EOFF",
        dbf_type: DbFieldType::Double,
        read_only: false,
    },
    FieldDesc {
        name: "ROFF",
        dbf_type: DbFieldType::Long,
        read_only: false,
    },
    FieldDesc {
        name: "ASLO",
        dbf_type: DbFieldType::Double,
        read_only: false,
    },
    FieldDesc {
        name: "AOFF",
        dbf_type: DbFieldType::Double,
        read_only: false,
    },
    FieldDesc {
        name: "SMOO",
        dbf_type: DbFieldType::Double,
        read_only: false,
    },
    FieldDesc {
        name: "ADEL",
        dbf_type: DbFieldType::Double,
        read_only: false,
    },
    FieldDesc {
        name: "MDEL",
        dbf_type: DbFieldType::Double,
        read_only: false,
    },
    FieldDesc {
        name: "AFTC",
        dbf_type: DbFieldType::Double,
        read_only: false,
    },
    FieldDesc {
        // aiRecord.dbd.pod: AFVL is special(SPC_NOMOD) — read-only to clients.
        name: "AFVL",
        dbf_type: DbFieldType::Double,
        read_only: true,
    },
    FieldDesc {
        name: "LALM",
        dbf_type: DbFieldType::Double,
        read_only: true,
    },
    FieldDesc {
        name: "ALST",
        dbf_type: DbFieldType::Double,
        read_only: true,
    },
    FieldDesc {
        name: "MLST",
        dbf_type: DbFieldType::Double,
        read_only: true,
    },
    FieldDesc {
        name: "INIT",
        dbf_type: DbFieldType::Char,
        read_only: true,
    },
    FieldDesc {
        name: "SIMM",
        dbf_type: DbFieldType::Short,
        read_only: false,
    },
    FieldDesc {
        name: "SIML",
        dbf_type: DbFieldType::String,
        read_only: false,
    },
    FieldDesc {
        name: "SIOL",
        dbf_type: DbFieldType::String,
        read_only: false,
    },
    FieldDesc {
        name: "SIMS",
        dbf_type: DbFieldType::Short,
        read_only: false,
    },
    FieldDesc {
        name: "SDLY",
        dbf_type: DbFieldType::Double,
        read_only: false,
    },
    FieldDesc {
        name: "SVAL",
        dbf_type: DbFieldType::Double,
        read_only: false,
    },
];

impl Record for AiRecord {
    fn record_type(&self) -> &'static str {
        "ai"
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
            // init stays false: first process() will skip smoothing (prime filter)
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

            // Step 3: Smoothing filter
            if self.smoo != 0.0 && self.init && self.val.is_finite() {
                self.val = v * (1.0 - self.smoo) + self.val * self.smoo;
            } else {
                self.val = v;
            }
        } // end skip_convert
        self.skip_convert = false;

        self.oraw = self.rval;
        self.init = true;
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
            "ROFF" => Some(EpicsValue::Long(self.roff)),
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
            "INIT" => Some(EpicsValue::Char(if self.init { 1 } else { 0 })),
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
            // SPC_LINCONV parity (aiRecord.c:181-200): LINR / EGUF / EGUL
            // are tagged `special(SPC_LINCONV)` in aiRecord.dbd. Every put
            // must (1) clear the smoothing-primed flag so the next
            // process() reprimes the SMOO filter under the new linearisation,
            // and (2) for LINR=LINEAR rebase `eoff` to `egul` (the C path
            // sets eoff=egul before calling the device-support
            // `special_linconv` callback; Rust ports usually don't carry
            // such a callback, so the eoff-rebase is the only visible
            // effect for soft-channel records).
            "LINR" => match value {
                EpicsValue::Short(v) => {
                    self.linr = v;
                    self.init = false;
                    if self.linr == 2 {
                        self.eoff = self.egul;
                    }
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
                    self.init = false;
                    if self.linr == 2 {
                        self.eoff = self.egul;
                    }
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch(name.into())),
            },
            "EGUL" => match value {
                EpicsValue::Double(v) => {
                    self.egul = v;
                    self.init = false;
                    if self.linr == 2 {
                        self.eoff = self.egul;
                    }
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
            "ROFF" => match value {
                EpicsValue::Long(v) => {
                    self.roff = v;
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

    fn field_list(&self) -> &'static [FieldDesc] {
        FIELDS
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

    /// C `aiRecord.c:460-465` posts `RVAL` with VAL's own `monitor_mask`,
    /// nested in `if (monitor_mask)`: RVAL is posted only when VAL is
    /// posted this cycle (alarm change or MDEL/ADEL crossing) and RVAL
    /// changed — never with a forced `DBE_VALUE | DBE_LOG`. Under a
    /// non-default MDEL the raw count can change while VAL stays inside the
    /// deadband, so RVAL must NOT post on the default aux path.
    fn fields_posted_with_value_mask(&self) -> &'static [&'static str] {
        &["RVAL"]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// SPC_LINCONV parity (aiRecord.c:181-200): writing LINR / EGUF / EGUL
    /// must clear the smoothing-primed flag so the next process() reprimes
    /// the SMOO filter under the new linearisation. Without this fix the
    /// SMOO low-pass blended pre-LINR-change and post-LINR-change values
    /// together, leaking the old engineering unit into the new one for
    /// every subsequent sample until lcnt or restart cleared it.
    #[test]
    fn linr_put_resets_init_flag_for_smoothing_prime() {
        let mut rec = AiRecord::default();
        rec.init = true; // simulate post-first-process primed state
        rec.smoo = 0.5;

        rec.put_field("LINR", EpicsValue::Short(2)).unwrap();

        assert!(
            !rec.init,
            "LINR put must clear init so next process reprimes SMOO"
        );
    }

    /// SPC_LINCONV parity (aiRecord.c:188-198): in LINR=LINEAR mode, the
    /// C path rebases `eoff = egul` before invoking the device-support
    /// `special_linconv` callback. Rust soft-channel ports carry no such
    /// callback, so the eoff-rebase is the only visible effect — but it
    /// is the one users actually depend on when retuning LINR or EGUL
    /// from CA/PVA.
    #[test]
    fn egul_put_rebases_eoff_under_linear_mode() {
        let mut rec = AiRecord::default();
        rec.linr = 2; // LINEAR
        rec.eoff = 1.5;

        rec.put_field("EGUL", EpicsValue::Double(7.25)).unwrap();

        assert_eq!(rec.eoff, 7.25, "EGUL put under LINEAR must set eoff=egul");
        assert_eq!(rec.egul, 7.25);
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
    /// early-return, aiRecord.c:432-437) but raises SOFT_ALARM/MAJOR via
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
