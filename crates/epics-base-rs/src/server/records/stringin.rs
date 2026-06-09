use crate::error::{CaError, CaResult};
use crate::server::record::{FieldDesc, MENU_YES_NO, ProcessOutcome, Record};
use crate::types::{DbFieldType, EpicsValue};

/// EPICS `MAX_STRING_SIZE` — `val`/`oval`/`sval` are fixed 40-byte
/// buffers in C `stringinRecord.c`; every copy truncates at 40.
const MAX_STRING_SIZE: usize = 40;

/// Truncate `s` to at most `max` bytes, snapping back to a UTF-8
/// char boundary. C `strncpy(dst, src, 40)` truncates at 39 + NUL.
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

/// Stringin record — a 40-byte (`MAX_STRING_SIZE`) string input.
///
/// C `stringinRecord.c`: `val`, `oval` and `sval` are fixed 40-byte
/// buffers; `strncpy(..., sizeof(prec->val))` truncates every copy at
/// 40. The Rust port uses `String`, so every VAL/OVAL write must be
/// truncated to 39 chars + implicit NUL to match.
pub struct StringinRecord {
    pub val: String,
    pub oval: String,
    pub simm: i16,
    pub siml: String,
    pub siol: String,
    pub sims: i16,
}

impl Default for StringinRecord {
    fn default() -> Self {
        Self {
            val: String::new(),
            oval: String::new(),
            simm: 0,
            siml: String::new(),
            siol: String::new(),
            sims: 0,
        }
    }
}

impl StringinRecord {
    pub fn new(val: &str) -> Self {
        Self {
            val: truncate_string(val),
            ..Default::default()
        }
    }
}

static STRINGIN_FIELDS: &[FieldDesc] = &[
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

impl Record for StringinRecord {
    fn record_type(&self) -> &'static str {
        "stringin"
    }

    fn field_list(&self) -> &'static [FieldDesc] {
        STRINGIN_FIELDS
    }

    /// `SIMM` is `DBF_MENU menu(menuYesNo)` (`stringinRecord.dbd.pod`): the
    /// two-choice NO/YES simulation menu. Served as `DBR_ENUM` with these
    /// labels. `SIMS`/`OLDSIMM` are shared menus resolved centrally.
    fn menu_field_choices(&self, field: &str) -> Option<&'static [&'static str]> {
        match field {
            "SIMM" => Some(MENU_YES_NO),
            _ => None,
        }
    }

    fn process(&mut self) -> CaResult<ProcessOutcome> {
        // C `stringinRecord.c::monitor` copies VAL into OVAL.
        self.oval = self.val.clone();
        Ok(ProcessOutcome::complete())
    }

    fn get_field(&self, name: &str) -> Option<EpicsValue> {
        match name {
            "VAL" => Some(EpicsValue::String(self.val.clone().into())),
            "OVAL" => Some(EpicsValue::String(self.oval.clone().into())),
            "SIMM" => Some(EpicsValue::Short(self.simm)),
            "SIML" => Some(EpicsValue::String(self.siml.clone().into())),
            "SIOL" => Some(EpicsValue::String(self.siol.clone().into())),
            "SIMS" => Some(EpicsValue::Short(self.sims)),
            _ => None,
        }
    }

    fn put_field(&mut self, name: &str, value: EpicsValue) -> CaResult<()> {
        match name {
            "VAL" => match value {
                // C truncates every VAL copy at MAX_STRING_SIZE (40).
                EpicsValue::String(s) => {
                    self.val = truncate_string(&s.as_str_lossy());
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch("VAL".into())),
            },
            "OVAL" => match value {
                EpicsValue::String(s) => {
                    self.oval = truncate_string(&s.as_str_lossy());
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
}
