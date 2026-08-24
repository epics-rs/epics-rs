use crate::error::{CaError, CaResult};
use crate::server::record::{MENU_YES_NO, ProcessOutcome, Record};
use crate::types::{EpicsValue, PvString};

// int64out: 64-bit integer output.
// CA limitation: served as DBR_DOUBLE over Channel Access (precision loss for |val|>2^53).
// Native i64 storage is lossless; precision is only lost at the CA wire boundary.
//
// Alarm threshold fields (HIHI/HIGH/LOW/LOLO/HHSV/HSV/LSV/LLSV) are intentionally absent
// from the field list so they route to RecordInstance::common.analog_alarm via
// put_common_field, matching the path used by longout/ao.
//
// Manually implements [`Record`] (rather than `#[derive(EpicsRecord)]`) so the
// `process()` hook can clamp VAL to the DRVH/DRVL drive-limit window — C parity
// with `int64outRecord.c::convert` (see [`Record::process`] below).
pub struct Int64outRecord {
    pub val: i64,
    pub egu: PvString,
    // HOPR/LOPR/DRVH/DRVL/HYST/IVOV/ADEL/MDEL are DBF_INT64
    // (int64outRecord.dbd.pod:176-386), not DBF_DOUBLE. Stored as i64 so the
    // string→native put parse keys on `epicsParseInt64` and REFUSES a fractional
    // or out-of-i64-range caput, matching C; served over CA as DBR_DOUBLE via
    // `EpicsValue::Int64`'s wire mapping, the same as VAL.
    pub hopr: i64,
    pub lopr: i64,
    pub drvh: i64,
    pub drvl: i64,
    pub hyst: i64,
    // LALM/ALST/MLST are DBF_INT64 too, but `special(SPC_NOMOD)`: read-only, so
    // no client put reaches the parse. LALM is the alarm ladder's own
    // `epicsInt64 lalm` (`int64outRecord.c:294`) — the threshold its hysteresis
    // arm compares against — so it holds that exactly; ALST/MLST stay doubles,
    // read by the monitor deadband engine.
    pub lalm: i64,
    pub ivoa: i16,
    pub ivov: i64,
    pub adel: i64,
    pub mdel: i64,
    pub alst: f64,
    pub mlst: f64,
    pub omsl: i16,
    pub dol: String,
    pub simm: i16,
    pub siml: String,
    pub siol: String,
    pub sims: i16,
    pub sdly: f64,
    /// Suppresses THIS cycle's `convert()` — C `int64outRecord.c:146` /
    /// `longoutRecord.c:155` `if (!status) convert(prec, value)`, whose convert
    /// is the DRVH/DRVL clamp. Set by
    /// [`Record::closed_loop_dol_read_failed`] and cleared by `process()`, so a
    /// dead closed-loop DOL leaves VAL exactly as the last successful cycle (or
    /// a client put) left it instead of re-clamping it into the drive window.
    pub skip_convert: bool,
}

impl Default for Int64outRecord {
    fn default() -> Self {
        Self {
            val: 0,
            egu: PvString::new(),
            hopr: 0,
            lopr: 0,
            drvh: 0,
            drvl: 0,
            hyst: 0,
            lalm: 0,
            ivoa: 0,
            ivov: 0,
            adel: 0,
            mdel: 0,
            alst: 0.0,
            mlst: 0.0,
            omsl: 0,
            dol: String::new(),
            simm: 0,
            siml: String::new(),
            siol: String::new(),
            sims: 0,
            sdly: -1.0,
            skip_convert: false,
        }
    }
}

impl Int64outRecord {
    pub fn new(val: i64) -> Self {
        Self {
            val,
            ..Default::default()
        }
    }
}

impl Record for Int64outRecord {
    fn record_type(&self) -> &'static str {
        "int64out"
    }

    /// C `int64outRecord.c:139-150`: the scalar `dbGetLink(&prec->dol, ..., &prec->val, 0, 0)`
    /// under `dol.type != CONSTANT && omsl == menuOmslclosed_loop`.
    fn fetches_dol_closed_loop(&self) -> bool {
        true
    }

    /// C `int64outRecord.c:135-143`: `process()` clears `udf` to FALSE only on a
    /// successful closed-loop DOL fetch (`if (prec->dol.type!=CONSTANT &&
    /// RTN_SUCCESS(status)) prec->udf=FALSE;`); the no-DOL arm reads the current
    /// VAL and leaves `udf` alone, so `checkAlarms` (`:298-299`) raises UDF_ALARM
    /// every cycle for a bare record. `udf` is never re-derived from the stored
    /// VAL, so int64out opts out of the framework's blanket per-cycle clear. The
    /// definers are a direct VAL put (`dbPut`, `field_io.rs`) and the DOL-apply
    /// site (`processing.rs`).
    fn clears_udf(&self) -> bool {
        false
    }

    /// C `int64outRecord.c:109-111`:
    /// `if (prec->dol.type == CONSTANT) {
    ///      if (recGblInitConstantLink(&prec->dol, DBF_INT64, &prec->val))
    ///          prec->udf = FALSE; }`
    /// The framework gate (`processing.rs`) excludes a constant DOL from the
    /// per-cycle closed-loop fetch (C `!dbLinkIsConstant`), so the init-seed
    /// owner is the only place the constant can reach VAL — and a record whose
    /// VAL came from it is DEFINED.
    fn constant_init_links(&self) -> Vec<crate::server::record::ConstantInitLink> {
        vec![crate::server::record::ConstantInitLink::dol_to_val(
            "DOL", "VAL",
        )]
    }

    /// C `int64outRecord.c::convert` (lines 418-423): clamp VAL into the
    /// drive-limit window `[DRVL, DRVH]` every process cycle, but only
    /// when `DRVH > DRVL` (equal limits = no clamping). DRVH/DRVL are
    /// DBF_INT64 in int64out's .dbd and are stored as i64 here, so the
    /// comparison and clamp are the direct integer clamp C's `convert`
    /// performs against the `epicsInt64 value`.
    fn process(&mut self) -> CaResult<ProcessOutcome> {
        if !self.skip_convert && self.drvh > self.drvl {
            if self.val > self.drvh {
                self.val = self.drvh;
            } else if self.val < self.drvl {
                self.val = self.drvl;
            }
        }
        self.skip_convert = false;
        Ok(ProcessOutcome::complete())
    }

    /// C `int64outRecord.c:146` — `if (!status) convert(prec, value)`: a failed
    /// closed-loop DOL read skips the convert, so the DRVH/DRVL clamp does not
    /// run and VAL keeps whatever it held.
    fn closed_loop_dol_read_failed(&mut self) {
        self.skip_convert = true;
    }

    /// `SIMM` is `DBF_MENU menu(menuYesNo)` (`int64outRecord.dbd.pod`): the
    /// integer records carry the two-choice NO/YES simulation menu, not the
    /// three-choice `menuSimm` used by the analog/binary/multibit records.
    /// Served as `DBR_ENUM` with these labels. `SIMS`/`OLDSIMM`/`OMSL`/
    /// `IVOA` are shared menus resolved centrally.
    fn menu_field_choices(&self, field: &str) -> Option<&'static [&'static str]> {
        match field {
            "SIMM" => Some(MENU_YES_NO),
            _ => None,
        }
    }

    fn get_field(&self, name: &str) -> Option<EpicsValue> {
        match name {
            "VAL" => Some(EpicsValue::Int64(self.val)),
            "EGU" => Some(EpicsValue::String(self.egu.clone())),
            "HOPR" => Some(EpicsValue::Int64(self.hopr)),
            "LOPR" => Some(EpicsValue::Int64(self.lopr)),
            "DRVH" => Some(EpicsValue::Int64(self.drvh)),
            "DRVL" => Some(EpicsValue::Int64(self.drvl)),
            "HYST" => Some(EpicsValue::Int64(self.hyst)),
            "LALM" => Some(EpicsValue::Int64(self.lalm)),
            "IVOA" => Some(EpicsValue::Short(self.ivoa)),
            "IVOV" => Some(EpicsValue::Int64(self.ivov)),
            "ADEL" => Some(EpicsValue::Int64(self.adel)),
            "MDEL" => Some(EpicsValue::Int64(self.mdel)),
            "ALST" => Some(EpicsValue::Double(self.alst)),
            "MLST" => Some(EpicsValue::Double(self.mlst)),
            "OMSL" => Some(EpicsValue::Short(self.omsl)),
            "DOL" => Some(EpicsValue::String(self.dol.clone().into())),
            "SIMM" => Some(EpicsValue::Short(self.simm)),
            "SIML" => Some(EpicsValue::String(self.siml.clone().into())),
            "SIOL" => Some(EpicsValue::String(self.siol.clone().into())),
            "SIMS" => Some(EpicsValue::Short(self.sims)),
            "SDLY" => Some(EpicsValue::Double(self.sdly)),
            _ => None,
        }
    }

    fn put_field(&mut self, name: &str, value: EpicsValue) -> CaResult<()> {
        self.validate_put(name, &value)?;
        match name {
            "VAL" => {
                if let EpicsValue::Int64(v) = value {
                    self.val = v;
                } else {
                    return Err(CaError::TypeMismatch("VAL".into()));
                }
            }
            "EGU" => {
                if let EpicsValue::String(v) = value {
                    self.egu = v;
                } else {
                    return Err(CaError::TypeMismatch("EGU".into()));
                }
            }
            "HOPR" => {
                if let EpicsValue::Int64(v) = value {
                    self.hopr = v;
                } else {
                    return Err(CaError::TypeMismatch("HOPR".into()));
                }
            }
            "LOPR" => {
                if let EpicsValue::Int64(v) = value {
                    self.lopr = v;
                } else {
                    return Err(CaError::TypeMismatch("LOPR".into()));
                }
            }
            "DRVH" => {
                if let EpicsValue::Int64(v) = value {
                    self.drvh = v;
                } else {
                    return Err(CaError::TypeMismatch("DRVH".into()));
                }
            }
            "DRVL" => {
                if let EpicsValue::Int64(v) = value {
                    self.drvl = v;
                } else {
                    return Err(CaError::TypeMismatch("DRVL".into()));
                }
            }
            "HYST" => {
                if let EpicsValue::Int64(v) = value {
                    self.hyst = v;
                } else {
                    return Err(CaError::TypeMismatch("HYST".into()));
                }
            }
            "LALM" => {
                if let EpicsValue::Int64(v) = value {
                    self.lalm = v;
                } else {
                    return Err(CaError::TypeMismatch("LALM".into()));
                }
            }
            "IVOA" => {
                if let EpicsValue::Short(v) = value {
                    self.ivoa = v;
                } else {
                    return Err(CaError::TypeMismatch("IVOA".into()));
                }
            }
            "IVOV" => {
                if let EpicsValue::Int64(v) = value {
                    self.ivov = v;
                } else {
                    return Err(CaError::TypeMismatch("IVOV".into()));
                }
            }
            "ADEL" => {
                if let EpicsValue::Int64(v) = value {
                    self.adel = v;
                } else {
                    return Err(CaError::TypeMismatch("ADEL".into()));
                }
            }
            "MDEL" => {
                if let EpicsValue::Int64(v) = value {
                    self.mdel = v;
                } else {
                    return Err(CaError::TypeMismatch("MDEL".into()));
                }
            }
            "ALST" => {
                self.alst = value
                    .to_f64()
                    .ok_or_else(|| CaError::TypeMismatch("ALST".into()))?;
            }
            "MLST" => {
                self.mlst = value
                    .to_f64()
                    .ok_or_else(|| CaError::TypeMismatch("MLST".into()))?;
            }
            "OMSL" => {
                if let EpicsValue::Short(v) = value {
                    self.omsl = v;
                } else {
                    return Err(CaError::TypeMismatch("OMSL".into()));
                }
            }
            "DOL" => {
                if let EpicsValue::String(v) = value {
                    self.dol = v.as_str_lossy().into_owned();
                } else {
                    return Err(CaError::TypeMismatch("DOL".into()));
                }
            }
            "SIMM" => {
                if let EpicsValue::Short(v) = value {
                    self.simm = v;
                } else {
                    return Err(CaError::TypeMismatch("SIMM".into()));
                }
            }
            "SIML" => {
                if let EpicsValue::String(v) = value {
                    self.siml = v.as_str_lossy().into_owned();
                } else {
                    return Err(CaError::TypeMismatch("SIML".into()));
                }
            }
            "SIOL" => {
                if let EpicsValue::String(v) = value {
                    self.siol = v.as_str_lossy().into_owned();
                } else {
                    return Err(CaError::TypeMismatch("SIOL".into()));
                }
            }
            "SIMS" => {
                if let EpicsValue::Short(v) = value {
                    self.sims = v;
                } else {
                    return Err(CaError::TypeMismatch("SIMS".into()));
                }
            }
            "SDLY" => {
                if let EpicsValue::Double(v) = value {
                    self.sdly = v;
                } else {
                    return Err(CaError::TypeMismatch("SDLY".into()));
                }
            }
            _ => return Err(CaError::FieldNotFound(name.to_string())),
        }
        self.on_put(name);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clamps_val_into_drive_window() {
        let mut r = Int64outRecord::new(0);
        r.drvl = -10;
        r.drvh = 10;
        r.val = 50;
        r.process().unwrap();
        assert_eq!(r.val, 10, "above DRVH clamps down to DRVH");
        r.val = -50;
        r.process().unwrap();
        assert_eq!(r.val, -10, "below DRVL clamps up to DRVL");
        r.val = 3;
        r.process().unwrap();
        assert_eq!(r.val, 3, "in-window value untouched");
    }

    #[test]
    fn equal_drive_limits_disable_clamp() {
        let mut r = Int64outRecord::new(0);
        r.drvl = 0;
        r.drvh = 0; // DRVH not > DRVL
        r.val = 99999;
        r.process().unwrap();
        assert_eq!(r.val, 99999, "DRVH == DRVL: no clamping (C parity)");
    }

    #[test]
    fn put_get_val_roundtrip() {
        let mut r = Int64outRecord::new(0);
        r.put_field("VAL", EpicsValue::Int64(1234567890123))
            .unwrap();
        assert_eq!(r.get_field("VAL"), Some(EpicsValue::Int64(1234567890123)));
    }
}
