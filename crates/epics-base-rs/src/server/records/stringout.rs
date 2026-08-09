use crate::error::{CaError, CaResult};
use crate::server::record::{MENU_POST, MENU_YES_NO, ProcessOutcome, Record};
use crate::types::{EpicsValue, PvString};

/// EPICS `MAX_STRING_SIZE` — `val`/`oval`/`ivov` are fixed 40-byte
/// buffers in C `stringoutRecord.c`.
const MAX_STRING_SIZE: usize = 40;

/// Truncate `s` to at most 39 payload bytes (40 incl. the implicit
/// NUL). Mirrors C `strncpy(dst, src, 40)`, which copies bytes with no
/// UTF-8 awareness, so the cut is on a raw byte boundary and a non-UTF-8
/// VAL keeps its bytes verbatim.
fn truncate_string(s: PvString) -> PvString {
    let max = MAX_STRING_SIZE - 1;
    if s.len() <= max {
        return s;
    }
    PvString::from_bytes(s.as_bytes()[..max].to_vec())
}

/// Stringout record — a 40-byte (`MAX_STRING_SIZE`) string output.
///
/// DOL (closed-loop source) and IVOA (invalid-output action) are
/// driven by the framework's generic output path: it reads DOL into
/// VAL when `OMSL == closed_loop`, and on `INVALID_ALARM` applies
/// IVOA, invoking [`Record::apply_invalid_output_value`] for IVOA=2
/// (`Set output to IVOV`). C `stringoutRecord.c::process` IVOA=2 just
/// does `strncpy(prec->val, prec->ivov, 40)`, which the default
/// `apply_invalid_output_value` (`set_val`) reproduces exactly.
pub struct StringoutRecord {
    pub val: PvString,
    pub oval: PvString,
    pub ivoa: i16,
    pub ivov: PvString,
    pub omsl: i16,
    pub dol: String,
    pub simm: i16,
    pub siml: String,
    pub siol: String,
    pub sims: i16,
    pub sdly: f64,
    /// `menu(stringoutPOST)` Post Value Monitors (0=On Change, 1=Always).
    pub mpst: i16,
    /// `menu(stringoutPOST)` Post Archive Monitors (0=On Change, 1=Always).
    pub apst: i16,
    /// C `monitor()`'s `strncmp(oval, val)` verdict for THIS cycle, captured
    /// in `process()` before OVAL is committed — the framework reads it
    /// afterwards, by which time `oval == val`.
    value_changed: bool,
}

/// `menu(stringoutPOST)` — `stringoutRecord.dbd.pod`: 0 = On Change, 1 = Always.
const MENU_POST_ALWAYS: i16 = 1;

impl Default for StringoutRecord {
    fn default() -> Self {
        Self {
            val: PvString::new(),
            oval: PvString::new(),
            ivoa: 0,
            ivov: PvString::new(),
            omsl: 0,
            dol: String::new(),
            simm: 0,
            siml: String::new(),
            siol: String::new(),
            sims: 0,
            sdly: -1.0,
            mpst: 0,
            apst: 0,
            value_changed: false,
        }
    }
}

impl StringoutRecord {
    pub fn new(val: &str) -> Self {
        Self {
            val: truncate_string(PvString::from(val)),
            ..Default::default()
        }
    }
}

impl Record for StringoutRecord {
    fn record_type(&self) -> &'static str {
        "stringout"
    }

    /// C reads the closed-loop DOL with a plain `dbGetLink(..., DBR_STRING,
    /// ...)` (`stringoutRecord.c:141`): an ENUM/MENU source delivers its
    /// state label, never the index digits (epics-base#183).
    fn input_link_read_as(
        &self,
        link_field: &str,
        _source: &crate::server::record::OutTarget,
    ) -> Option<crate::server::record::LinkReadAs> {
        Some(match link_field {
            "DOL" => crate::server::record::LinkReadAs::String,
            _ => crate::server::record::LinkReadAs::Native,
        })
    }

    /// `SIMM` is `DBF_MENU menu(menuYesNo)` (`stringoutRecord.dbd.pod`): the
    /// two-choice NO/YES simulation menu. `MPST`/`APST` are
    /// `menu(stringoutPOST)` (`stringoutRecord.dbd.pod:19-22,128-139`), whose
    /// value order ("On Change", "Always") matches `menu(menuPost)`. Served
    /// as `DBR_ENUM` with these labels. `SIMS`/`OLDSIMM`/`OMSL`/`IVOA` are
    /// shared menus resolved centrally.
    fn menu_field_choices(&self, field: &str) -> Option<&'static [&'static str]> {
        match field {
            "SIMM" => Some(MENU_YES_NO),
            "MPST" | "APST" => Some(MENU_POST),
            _ => None,
        }
    }

    /// C `stringoutRecord.c::init_record` applies a constant DOL to VAL
    /// once via `recGblInitConstantLink(&prec->dol, DBF_STRING, prec->val)`.
    /// The framework gate (`processing.rs`) excludes a constant DOL from
    /// the per-cycle closed-loop fetch (C `!dbLinkIsConstant`), so the
    /// constant text must be seeded here. A bare-numeric (`5`) or quoted
    /// (`"text"`) DOL parses to `ParsedLink::Constant`; its text is copied
    /// verbatim into the `DBF_STRING` VAL (truncated to the field size).
    /// C `stringoutRecord.c:113-114`:
    /// `if (recGblInitConstantLink(&prec->dol, DBF_STRING, prec->val))
    ///      prec->udf = FALSE;`
    /// The framework gate excludes a constant DOL from the per-cycle
    /// closed-loop fetch, so the init-seed owner is the only place the constant
    /// can reach VAL — and a record whose VAL came from it is DEFINED.
    fn constant_init_links(&self) -> Vec<crate::server::record::ConstantInitLink> {
        vec![crate::server::record::ConstantInitLink::dol_to_val(
            "DOL", "VAL",
        )]
    }

    /// C `stringoutRecord.c::monitor` (:211-227) has no MDEL/ADEL deadband: the
    /// VAL post is gated on `strncmp(oval, val)` plus the MPST/APST "Always"
    /// override — identical to `stringin`. See `stringin.rs` for the analog
    /// deadband owner this replaces.
    fn uses_monitor_deadband(&self) -> bool {
        false
    }

    fn monitor_value_changed(&self) -> Option<bool> {
        Some(self.value_changed)
    }

    /// C `stringoutRecord.c:138-144`: `process()` clears `udf` to FALSE only
    /// on a successful non-constant closed-loop DOL fetch (`if
    /// (!dbLinkIsConstant(&prec->dol) && !status) prec->udf=FALSE;`); with no
    /// such fetch this cycle, `udf` is left untouched and `if (prec->udf ==
    /// TRUE) recGblSetSevr(prec,UDF_ALARM,prec->udfs);` (`:146-148`) raises it
    /// every cycle for a bare record. `udf` is never re-derived from the
    /// stored VAL, so stringout opts out of the framework's blanket
    /// per-cycle clear. The definers are a direct VAL put (`dbPut`,
    /// `field_io.rs`) and the DOL-apply site (`processing.rs`).
    fn clears_udf(&self) -> bool {
        false
    }

    /// C `stringoutRecord.c:146` tests `if (prec->udf == TRUE)` — exact-one.
    /// With `clears_udf() == false`, a direct `caput X.UDF 255` (or `-1`,
    /// stored `255`) leaves `udf == 255` at the alarm check, and `255 != TRUE`,
    /// so C raises NO UDF_ALARM — STAT/SEVR stay `NO_ALARM`. See
    /// [`Record::udf_alarm_on_exact_one`].
    fn udf_alarm_on_exact_one(&self) -> bool {
        true
    }

    /// C: `if (mpst == stringoutPOST_Always) monitor_mask |= DBE_VALUE;`
    /// `if (apst == stringoutPOST_Always) monitor_mask |= DBE_LOG;`
    fn monitor_always_post(&self) -> (bool, bool) {
        (self.mpst == MENU_POST_ALWAYS, self.apst == MENU_POST_ALWAYS)
    }

    fn process(&mut self) -> CaResult<ProcessOutcome> {
        // C `stringoutRecord.c::monitor` copies VAL into OVAL only on the
        // `strncmp` mismatch that raises DBE_VALUE|DBE_LOG. Capture the verdict
        // before the copy erases it — the framework reads
        // `monitor_value_changed()` after `process()` returns.
        self.value_changed = self.oval != self.val;
        if self.value_changed {
            self.oval = self.val.clone();
        }
        Ok(ProcessOutcome::complete())
    }

    fn get_field(&self, name: &str) -> Option<EpicsValue> {
        match name {
            "VAL" => Some(EpicsValue::String(self.val.clone())),
            "OVAL" => Some(EpicsValue::String(self.oval.clone())),
            "IVOA" => Some(EpicsValue::Short(self.ivoa)),
            "IVOV" => Some(EpicsValue::String(self.ivov.clone())),
            "OMSL" => Some(EpicsValue::Short(self.omsl)),
            "DOL" => Some(EpicsValue::String(self.dol.clone().into())),
            "SIMM" => Some(EpicsValue::Short(self.simm)),
            "SIML" => Some(EpicsValue::String(self.siml.clone().into())),
            "SIOL" => Some(EpicsValue::String(self.siol.clone().into())),
            "SIMS" => Some(EpicsValue::Short(self.sims)),
            "SDLY" => Some(EpicsValue::Double(self.sdly)),
            "MPST" => Some(EpicsValue::Short(self.mpst)),
            "APST" => Some(EpicsValue::Short(self.apst)),
            _ => None,
        }
    }

    fn put_field(&mut self, name: &str, value: EpicsValue) -> CaResult<()> {
        match name {
            // C truncates VAL/OVAL/IVOV copies at MAX_STRING_SIZE (40).
            "VAL" => match value {
                EpicsValue::String(s) => {
                    self.val = truncate_string(s);
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch("VAL".into())),
            },
            "OVAL" => match value {
                EpicsValue::String(s) => {
                    self.oval = truncate_string(s);
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch("OVAL".into())),
            },
            "IVOA" => match value {
                EpicsValue::Short(v) => {
                    self.ivoa = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch("IVOA".into())),
            },
            "IVOV" => match value {
                EpicsValue::String(s) => {
                    self.ivov = truncate_string(s);
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch("IVOV".into())),
            },
            "OMSL" => match value {
                EpicsValue::Short(v) => {
                    self.omsl = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch("OMSL".into())),
            },
            "DOL" => match value {
                EpicsValue::String(s) => {
                    self.dol = s.as_str_lossy().into_owned();
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch("DOL".into())),
            },
            "SIMM" => match value {
                EpicsValue::Short(v) => {
                    self.simm = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch("SIMM".into())),
            },
            "SIML" => match value {
                EpicsValue::String(s) => {
                    self.siml = s.as_str_lossy().into_owned();
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch("SIML".into())),
            },
            "SIOL" => match value {
                EpicsValue::String(s) => {
                    self.siol = s.as_str_lossy().into_owned();
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch("SIOL".into())),
            },
            "SIMS" => match value {
                EpicsValue::Short(v) => {
                    self.sims = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch("SIMS".into())),
            },
            "SDLY" => match value {
                EpicsValue::Double(v) => {
                    self.sdly = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch("SDLY".into())),
            },
            "MPST" => match value {
                EpicsValue::Short(v) => {
                    self.mpst = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch("MPST".into())),
            },
            "APST" => match value {
                EpicsValue::Short(v) => {
                    self.apst = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch("APST".into())),
            },
            _ => Err(CaError::FieldNotFound(name.to_string())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// a VAL longer than 39 chars truncates to 39 + NUL.
    #[test]
    fn val_truncated_to_max_string_size() {
        let mut rec = StringoutRecord::default();
        rec.put_field("VAL", EpicsValue::String("z".repeat(80).into()))
            .unwrap();
        assert_eq!(rec.val.len(), 39);
    }

    /// MPST/APST are `menu(stringoutPOST)` served as DBR_ENUM with the
    /// wire-visible "On Change"/"Always" labels in `.dbd` value order.
    #[test]
    fn mpst_apst_snapshot_is_enum_with_post_labels() {
        use crate::server::record::RecordInstance;
        let mut rec = StringoutRecord::default();
        rec.put_field("APST", EpicsValue::Short(1)).unwrap();
        assert_eq!(rec.get_field("APST"), Some(EpicsValue::Short(1)));
        let inst = RecordInstance::new("SO:APST".into(), rec);
        let snap = inst.snapshot_for_field("APST").unwrap();
        assert_eq!(snap.value, EpicsValue::Enum(1));
        assert_eq!(
            snap.enums.as_ref().unwrap().strings,
            vec!["On Change", "Always"]
        );
    }

    /// IVOA=2 (set output to IVOV) copies IVOV into VAL.
    #[test]
    fn ivoa_set_to_ivov_copies_into_val() {
        let mut rec = StringoutRecord::default();
        rec.ivov = "fault".into();
        rec.apply_invalid_output_value(EpicsValue::String("fault".into()))
            .unwrap();
        assert_eq!(rec.val, "fault");
    }
}
