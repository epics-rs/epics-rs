use crate::error::{CaError, CaResult};
use crate::server::record::{FieldDesc, ProcessOutcome, Record};
use crate::types::{DbFieldType, EpicsValue};

/// EPICS `MAX_STRING_SIZE` — `val`/`oval`/`ivov` are fixed 40-byte
/// buffers in C `stringoutRecord.c`.
const MAX_STRING_SIZE: usize = 40;

/// Truncate `s` to at most 39 bytes (40 incl. NUL), snapping back to
/// a UTF-8 char boundary. Mirrors C `strncpy(dst, src, 40)`.
fn truncate_string(s: &str) -> String {
    let max = MAX_STRING_SIZE - 1;
    if s.len() <= max {
        return s.to_string();
    }
    let trunc = (0..=max)
        .rev()
        .find(|&i| s.is_char_boundary(i))
        .unwrap_or(0);
    s[..trunc].to_string()
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
    pub val: String,
    pub oval: String,
    pub ivoa: i16,
    pub ivov: String,
    pub omsl: i16,
    pub dol: String,
    pub simm: i16,
    pub siml: String,
    pub siol: String,
    pub sims: i16,
}

impl Default for StringoutRecord {
    fn default() -> Self {
        Self {
            val: String::new(),
            oval: String::new(),
            ivoa: 0,
            ivov: String::new(),
            omsl: 0,
            dol: String::new(),
            simm: 0,
            siml: String::new(),
            siol: String::new(),
            sims: 0,
        }
    }
}

impl StringoutRecord {
    pub fn new(val: &str) -> Self {
        Self {
            val: truncate_string(val),
            ..Default::default()
        }
    }
}

static STRINGOUT_FIELDS: &[FieldDesc] = &[
    FieldDesc {
        name: "VAL",
        dbf_type: DbFieldType::String,
        read_only: false,
    },
    FieldDesc {
        name: "OVAL",
        dbf_type: DbFieldType::String,
        read_only: true,
    },
    FieldDesc {
        name: "IVOA",
        dbf_type: DbFieldType::Short,
        read_only: false,
    },
    FieldDesc {
        name: "IVOV",
        dbf_type: DbFieldType::String,
        read_only: false,
    },
    FieldDesc {
        name: "OMSL",
        dbf_type: DbFieldType::Short,
        read_only: false,
    },
    FieldDesc {
        name: "DOL",
        dbf_type: DbFieldType::String,
        read_only: false,
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
];

impl Record for StringoutRecord {
    fn record_type(&self) -> &'static str {
        "stringout"
    }

    fn field_list(&self) -> &'static [FieldDesc] {
        STRINGOUT_FIELDS
    }

    fn process(&mut self) -> CaResult<ProcessOutcome> {
        // C `stringoutRecord.c::monitor` copies VAL into OVAL.
        self.oval = self.val.clone();
        Ok(ProcessOutcome::complete())
    }

    fn get_field(&self, name: &str) -> Option<EpicsValue> {
        match name {
            "VAL" => Some(EpicsValue::String(self.val.clone())),
            "OVAL" => Some(EpicsValue::String(self.oval.clone())),
            "IVOA" => Some(EpicsValue::Short(self.ivoa)),
            "IVOV" => Some(EpicsValue::String(self.ivov.clone())),
            "OMSL" => Some(EpicsValue::Short(self.omsl)),
            "DOL" => Some(EpicsValue::String(self.dol.clone())),
            "SIMM" => Some(EpicsValue::Short(self.simm)),
            "SIML" => Some(EpicsValue::String(self.siml.clone())),
            "SIOL" => Some(EpicsValue::String(self.siol.clone())),
            "SIMS" => Some(EpicsValue::Short(self.sims)),
            _ => None,
        }
    }

    fn put_field(&mut self, name: &str, value: EpicsValue) -> CaResult<()> {
        match name {
            // C truncates VAL/OVAL/IVOV copies at MAX_STRING_SIZE (40).
            "VAL" => match value {
                EpicsValue::String(s) => {
                    self.val = truncate_string(&s);
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch("VAL".into())),
            },
            "OVAL" => match value {
                EpicsValue::String(s) => {
                    self.oval = truncate_string(&s);
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
                    self.ivov = truncate_string(&s);
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
                    self.dol = s;
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
                    self.siml = s;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch("SIML".into())),
            },
            "SIOL" => match value {
                EpicsValue::String(s) => {
                    self.siol = s;
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
            _ => Err(CaError::FieldNotFound(name.to_string())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// H-10: a VAL longer than 39 chars truncates to 39 + NUL.
    #[test]
    fn val_truncated_to_max_string_size() {
        let mut rec = StringoutRecord::default();
        rec.put_field("VAL", EpicsValue::String("z".repeat(80)))
            .unwrap();
        assert_eq!(rec.val.len(), 39);
    }

    /// M-7: IVOA=2 (set output to IVOV) copies IVOV into VAL.
    #[test]
    fn ivoa_set_to_ivov_copies_into_val() {
        let mut rec = StringoutRecord::default();
        rec.ivov = "fault".into();
        rec.apply_invalid_output_value(EpicsValue::String("fault".into()))
            .unwrap();
        assert_eq!(rec.val, "fault");
    }
}
