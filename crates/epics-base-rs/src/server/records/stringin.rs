use crate::error::{CaError, CaResult};
use crate::server::record::{MENU_POST, MENU_YES_NO, ProcessOutcome, Record};
use crate::types::{EpicsValue, PvString};

/// EPICS `MAX_STRING_SIZE` — `val`/`oval`/`sval` are fixed 40-byte
/// buffers in C `stringinRecord.c`; every copy truncates at 40.
const MAX_STRING_SIZE: usize = 40;

/// Truncate `s` to at most `MAX_STRING_SIZE - 1` bytes. C
/// `strncpy(dst, src, sizeof(prec->val))` copies 39 payload bytes + an
/// implicit NUL byte for byte with no UTF-8 awareness, so the cut is on
/// a raw byte boundary and a non-UTF-8 VAL keeps its bytes verbatim.
fn truncate_string(s: PvString) -> PvString {
    let max = MAX_STRING_SIZE - 1;
    if s.len() <= max {
        return s;
    }
    PvString::from_bytes(s.as_bytes()[..max].to_vec())
}

/// Stringin record — a 40-byte (`MAX_STRING_SIZE`) string input.
///
/// C `stringinRecord.c`: `val`, `oval` and `sval` are fixed 40-byte
/// buffers; `strncpy(..., sizeof(prec->val))` truncates every copy at
/// 40. The Rust port uses `String`, so every VAL/OVAL write must be
/// truncated to 39 chars + implicit NUL to match.
pub struct StringinRecord {
    pub val: PvString,
    pub oval: PvString,
    pub simm: i16,
    pub siml: String,
    pub siol: String,
    pub sims: i16,
    pub sdly: f64,
    /// `menu(stringinPOST)` Post Value Monitors (0=On Change, 1=Always).
    pub mpst: i16,
    /// `menu(stringinPOST)` Post Archive Monitors (0=On Change, 1=Always).
    pub apst: i16,
    /// C `monitor()`'s `strncmp(oval, val)` verdict for THIS cycle, captured
    /// in `process()` before OVAL is committed — the framework reads it
    /// afterwards, by which time `oval == val`.
    value_changed: bool,
}

/// `menu(stringinPOST)` — `stringinRecord.dbd.pod`: 0 = On Change, 1 = Always.
const MENU_POST_ALWAYS: i16 = 1;

impl Default for StringinRecord {
    fn default() -> Self {
        Self {
            val: PvString::new(),
            oval: PvString::new(),
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

impl StringinRecord {
    pub fn new(val: &str) -> Self {
        Self {
            val: truncate_string(PvString::from(val)),
            ..Default::default()
        }
    }
}

impl Record for StringinRecord {
    fn record_type(&self) -> &'static str {
        "stringin"
    }

    /// C reads INP (`devSiSoft.c:53`) and SIOL (`stringinRecord.c:208`) with
    /// a plain `dbGetLink(..., DBR_STRING, ...)`: an ENUM/MENU source
    /// delivers its state label, never the index digits (epics-base#183).
    fn input_link_read_as(
        &self,
        link_field: &str,
        _source: &crate::server::record::OutTarget,
    ) -> Option<crate::server::record::LinkReadAs> {
        Some(match link_field {
            "INP" | "SIOL" => crate::server::record::LinkReadAs::String,
            _ => crate::server::record::LinkReadAs::Native,
        })
    }

    /// `SIMM` is `DBF_MENU menu(menuYesNo)` (`stringinRecord.dbd.pod`): the
    /// two-choice NO/YES simulation menu. `MPST`/`APST` are
    /// `menu(stringinPOST)` (`stringinRecord.dbd.pod:21-24,95-107`), whose
    /// value order ("On Change", "Always") matches `menu(menuPost)`. Served
    /// as `DBR_ENUM` with these labels. `SIMS`/`OLDSIMM` are shared menus
    /// resolved centrally.
    fn menu_field_choices(&self, field: &str) -> Option<&'static [&'static str]> {
        match field {
            "SIMM" => Some(MENU_YES_NO),
            "MPST" | "APST" => Some(MENU_POST),
            _ => None,
        }
    }

    /// C `stringinRecord.c::monitor` (:176-188) has no MDEL/ADEL deadband: the
    /// VAL post is gated on `strncmp(oval, val)` plus the MPST/APST "Always"
    /// override. Without this the record fell into the analog deadband owner,
    /// which posts everything a `to_f64()` cannot measure — one VAL event per
    /// subscriber per cycle on an unchanging string — and put a *numeric-looking*
    /// string ("12") through the MDEL comparison, where C's `strncmp` sees
    /// "12" → "12.0" as a change.
    fn uses_monitor_deadband(&self) -> bool {
        false
    }

    fn monitor_value_changed(&self) -> Option<bool> {
        Some(self.value_changed)
    }

    /// C: `if (mpst == stringinPOST_Always) monitor_mask |= DBE_VALUE;`
    /// `if (apst == stringinPOST_Always) monitor_mask |= DBE_LOG;`
    fn monitor_always_post(&self) -> (bool, bool) {
        (self.mpst == MENU_POST_ALWAYS, self.apst == MENU_POST_ALWAYS)
    }

    /// `stringinRecord.c::process` has NO unconditional UDF re-derive — unlike
    /// `aiRecord.c:161` it never runs `prec->udf = isnan(prec->val)`. UDF is
    /// cleared ONLY inside `devSiSoft.c::read_stringin` on a real read:
    /// `if (!status && !dbLinkIsConstant(&prec->inp)) prec->udf = FALSE`
    /// (and the SIOL simulation branch, `stringinRecord.c:209-211`). So a
    /// process cycle that sources nothing — e.g. a `caput .UDF 1` driving a
    /// Passive record with a constant/empty INP — must leave the client's UDF
    /// put intact. The framework clears UDF on a genuine soft read via
    /// `device_did_compute`; this opts out of the per-cycle blanket re-derive,
    /// exactly like `stringout`/`lso`/`bo`/`longout`.
    fn clears_udf(&self) -> bool {
        false
    }

    /// `stringinRecord.c` has NO `recGblCheckUdf` / `UDF_ALARM` (unlike
    /// `stringoutRecord.c:147`): an undefined stringin raises no alarm from UDF
    /// (softIoc: `record(stringin,"X"){}` → UDF 1, STAT/SEVR = NO_ALARM). With
    /// `clears_udf` false, UDF can now legitimately stay 1, so this MUST be
    /// false or `rec_gbl_check_udf` would invent an alarm C never raises.
    fn raises_udf_alarm(&self) -> bool {
        false
    }

    fn process(&mut self) -> CaResult<ProcessOutcome> {
        // C `stringinRecord.c::monitor` copies VAL into OVAL — but only on the
        // `strncmp` mismatch that also raises DBE_VALUE|DBE_LOG. Capture the
        // verdict here: the framework's monitor gate reads
        // `monitor_value_changed()` after `process()` returns, by which point
        // an unconditional copy would have erased it.
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
            "VAL" => match value {
                // C truncates every VAL copy at MAX_STRING_SIZE (40).
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

    /// a VAL longer than 39 chars is truncated to 39 + NUL.
    #[test]
    fn val_truncated_to_max_string_size() {
        let long = "x".repeat(100);
        let mut rec = StringinRecord::default();
        rec.put_field("VAL", EpicsValue::String(long.into()))
            .unwrap();
        assert_eq!(rec.val.len(), 39, "VAL capped at MAX_STRING_SIZE-1");
    }

    /// A 39-char string is kept whole.
    #[test]
    fn val_at_limit_kept_whole() {
        let s = "y".repeat(39);
        let rec = StringinRecord::new(&s);
        assert_eq!(rec.val.len(), 39);
    }

    /// MPST/APST are `menu(stringinPOST)` served as DBR_ENUM: the base
    /// snapshot path promotes the stored Short to `Enum` and attaches the
    /// wire-visible "On Change"/"Always" labels in `.dbd` value order.
    #[test]
    fn mpst_apst_snapshot_is_enum_with_post_labels() {
        use crate::server::record::RecordInstance;
        let mut rec = StringinRecord::default();
        rec.put_field("MPST", EpicsValue::Short(1)).unwrap();
        rec.put_field("APST", EpicsValue::Short(0)).unwrap();
        assert_eq!(rec.get_field("MPST"), Some(EpicsValue::Short(1)));
        let inst = RecordInstance::new("SI:MPST".into(), rec);
        let snap = inst.snapshot_for_field("MPST").unwrap();
        assert_eq!(snap.value, EpicsValue::Enum(1));
        assert_eq!(
            snap.enums.as_ref().unwrap().strings,
            vec!["On Change", "Always"]
        );
    }

    /// A non-UTF-8 VAL (`0xff 0x00 0x80`) put through the field path is
    /// stored and served back byte for byte, never U+FFFD-mangled. The C
    /// `stringinRecord` VAL is a fixed `char[40]`, so a non-UTF-8 value
    /// round-trips unchanged (the byte-based truncate keeps the raw bytes).
    #[test]
    fn val_preserves_non_utf8_bytes() {
        let mut rec = StringinRecord::default();
        let raw = vec![0xffu8, 0x00, 0x80];
        rec.put_field("VAL", EpicsValue::String(PvString::from_bytes(raw.clone())))
            .expect("VAL put");
        match rec.get_field("VAL") {
            Some(EpicsValue::String(s)) => assert_eq!(
                s.as_bytes(),
                raw.as_slice(),
                "VAL must round-trip the raw bytes, not lossily decode them"
            ),
            other => panic!("expected EpicsValue::String, got {other:?}"),
        }
    }
}
