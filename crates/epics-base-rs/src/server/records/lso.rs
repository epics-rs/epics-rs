use crate::error::{CaError, CaResult};
use crate::server::record::{FieldDesc, ProcessOutcome, Record};
use crate::types::{DbFieldType, EpicsValue};

/// EPICS `MAX_STRING_SIZE` — DBR_STRING buffers are 40 bytes.
const MAX_STRING_SIZE: usize = 40;

/// Truncate `s` to at most `max` bytes, snapping back to a UTF-8
/// char boundary so the result is always valid UTF-8.
fn truncate_utf8(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let trunc = (0..=max)
        .rev()
        .find(|&i| s.is_char_boundary(i))
        .unwrap_or(0);
    s[..trunc].to_string()
}

// Long string output record (EPICS 7).
// Native CA type is DBR_CHAR array; SIZV (default 256) sets the max byte count.
pub struct LsoRecord {
    pub val: String,
    pub oval: String,
    pub sizv: u16,
    pub len: u32,
    pub olen: u32,
    pub ivoa: i16,
    pub ivov: String,
    pub omsl: i16,
    pub dol: String,
    pub simm: i16,
    pub siml: String,
    pub siol: String,
    pub sims: i16,
}

impl Default for LsoRecord {
    fn default() -> Self {
        Self {
            val: String::new(),
            oval: String::new(),
            sizv: 256,
            // C `lsoRecord.c:62-64`: `prec->len = 0; prec->olen = 0;`.
            len: 0,
            olen: 0,
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

impl LsoRecord {
    pub fn new(val: &str) -> Self {
        let v = val.to_string();
        let len = if v.is_empty() {
            0
        } else {
            (v.len() + 1).min(256) as u32
        };
        Self {
            val: v,
            len,
            ..Default::default()
        }
    }

    fn clamped(&self) -> String {
        let max = (self.sizv as usize).saturating_sub(1);
        truncate_utf8(&self.val, max)
    }
}

static LSO_FIELDS: &[FieldDesc] = &[
    FieldDesc {
        name: "VAL",
        dbf_type: DbFieldType::Char,
        read_only: false,
    },
    FieldDesc {
        name: "OVAL",
        dbf_type: DbFieldType::Char,
        read_only: true,
    },
    FieldDesc {
        name: "SIZV",
        dbf_type: DbFieldType::Short,
        read_only: false,
    },
    FieldDesc {
        name: "LEN",
        dbf_type: DbFieldType::Long,
        read_only: true,
    },
    FieldDesc {
        name: "OLEN",
        dbf_type: DbFieldType::Long,
        read_only: true,
    },
    FieldDesc {
        name: "IVOA",
        dbf_type: DbFieldType::Short,
        read_only: false,
    },
    FieldDesc {
        name: "IVOV",
        dbf_type: DbFieldType::Char,
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

impl Record for LsoRecord {
    fn record_type(&self) -> &'static str {
        "lso"
    }

    fn field_list(&self) -> &'static [FieldDesc] {
        LSO_FIELDS
    }

    // C recLso.c IVOA=set_to_IVOV: oval = ivov (string copy); val = oval.
    fn apply_invalid_output_value(&mut self, ivov: EpicsValue) -> CaResult<()> {
        self.put_field("OVAL", ivov.clone())?;
        self.put_field("VAL", ivov)
    }

    fn uses_monitor_deadband(&self) -> bool {
        false
    }

    fn process(&mut self) -> CaResult<ProcessOutcome> {
        // C `lsoRecord.c::monitor` (lines 244-256): copy OVAL and bump
        // OLEN only when the value actually changed. LEN is set when
        // VAL is written (C `special`, Rust `put_field`); `process()`
        // must not recompute it.
        if self.len != self.olen || self.oval != self.val {
            self.oval = self.val.clone();
            self.olen = self.len;
        }
        Ok(ProcessOutcome::complete())
    }

    fn val(&self) -> Option<EpicsValue> {
        Some(EpicsValue::CharArray(self.clamped().into_bytes()))
    }

    fn get_field(&self, name: &str) -> Option<EpicsValue> {
        match name {
            "VAL" => Some(EpicsValue::CharArray(self.clamped().into_bytes())),
            "OVAL" => Some(EpicsValue::CharArray(self.oval.clone().into_bytes())),
            "SIZV" => Some(EpicsValue::Short(self.sizv as i16)),
            "LEN" => Some(EpicsValue::Long(self.len as i32)),
            "OLEN" => Some(EpicsValue::Long(self.olen as i32)),
            "IVOA" => Some(EpicsValue::Short(self.ivoa)),
            "IVOV" => Some(EpicsValue::CharArray(self.ivov.clone().into_bytes())),
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
            "VAL" => {
                // DBR_STRING-typed put caps at MAX_STRING_SIZE (40);
                // DBR_CHAR long-string put is bounded only by SIZV.
                let mut s = match value {
                    EpicsValue::String(s) => truncate_utf8(&s, MAX_STRING_SIZE - 1),
                    EpicsValue::CharArray(bytes) => {
                        let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
                        String::from_utf8_lossy(&bytes[..end]).into_owned()
                    }
                    _ => return Err(CaError::TypeMismatch("VAL".into())),
                };
                let max = (self.sizv as usize).saturating_sub(1);
                if s.len() > max {
                    s = truncate_utf8(&s, max);
                }
                self.val = s;
                self.len = (self.val.len() + 1) as u32;
            }
            "SIZV" => {
                if let EpicsValue::Short(v) = value {
                    // C `lsoRecord.c:51-58`: SIZV clamps to [16, 0x7fff].
                    self.sizv = (v as i32).clamp(16, 0x7fff) as u16;
                } else {
                    return Err(CaError::TypeMismatch("SIZV".into()));
                }
            }
            "IVOA" => {
                if let EpicsValue::Short(v) = value {
                    self.ivoa = v;
                } else {
                    return Err(CaError::TypeMismatch("IVOA".into()));
                }
            }
            "IVOV" => match value {
                EpicsValue::String(s) => self.ivov = s,
                EpicsValue::CharArray(bytes) => {
                    let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
                    self.ivov = String::from_utf8_lossy(&bytes[..end]).into_owned();
                }
                _ => return Err(CaError::TypeMismatch("IVOV".into())),
            },
            "OMSL" => {
                if let EpicsValue::Short(v) = value {
                    self.omsl = v;
                } else {
                    return Err(CaError::TypeMismatch("OMSL".into()));
                }
            }
            "DOL" => {
                if let EpicsValue::String(v) = value {
                    self.dol = v;
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
                    self.siml = v;
                } else {
                    return Err(CaError::TypeMismatch("SIML".into()));
                }
            }
            "SIOL" => {
                if let EpicsValue::String(v) = value {
                    self.siol = v;
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
            _ => return Err(CaError::FieldNotFound(name.to_string())),
        }
        Ok(())
    }
}
