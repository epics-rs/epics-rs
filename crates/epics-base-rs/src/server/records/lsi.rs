use crate::error::{CaError, CaResult};
use crate::server::record::{FieldDesc, ProcessOutcome, Record};
use crate::types::{DbFieldType, EpicsValue};

/// EPICS `MAX_STRING_SIZE` — DBR_STRING buffers are 40 bytes.
const MAX_STRING_SIZE: usize = 40;

/// `menuPost_Always` — index 1 of the `menuPost` menu
/// (`menuPost.dbd.pod`: `choice(menuPost_OnChange, ...)` = 0,
/// `choice(menuPost_Always, ...)` = 1). MPST/APST default to
/// `menuPost_OnChange` (0).
const MENU_POST_ALWAYS: i16 = 1;

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

// Long string input record (EPICS 7).
// Native CA type is DBR_CHAR array; SIZV (default 256) sets the max byte count.
// LEN reports the current string length (number of bytes including NUL terminator).
pub struct LsiRecord {
    pub val: String,
    pub oval: String,
    pub sizv: u16,
    pub len: u32,
    pub olen: u32,
    pub simm: i16,
    pub siml: String,
    pub siol: String,
    pub sims: i16,
    /// `menuPost` Post Value Monitors: `menuPost_OnChange` (0, default)
    /// posts DBE_VALUE only on a real change; `menuPost_Always` (1) posts
    /// DBE_VALUE every write (C `lsiRecord.dbd.pod` MPST, monitor:
    /// `if (mpst == menuPost_Always) events |= DBE_VALUE;`).
    pub mpst: i16,
    /// `menuPost` Post Archive Monitors: same as [`Self::mpst`] for the
    /// DBE_LOG (archive) mask (C `lsiRecord.dbd.pod` APST, monitor:
    /// `if (apst == menuPost_Always) events |= DBE_LOG;`).
    pub apst: i16,
    /// Per-cycle scratch: did VAL change on the most recent `process()`?
    /// Captured BEFORE `process()` commits `oval`/`olen` so the
    /// framework's post-process monitor gate can see it. C
    /// `lsiRecord.c::monitor`: `len != olen || memcmp(oval, val, len)`.
    value_changed: bool,
}

impl Default for LsiRecord {
    fn default() -> Self {
        Self {
            val: String::new(),
            oval: String::new(),
            sizv: 256,
            // C `lsiRecord.c:58-60`: `prec->len = 0; prec->olen = 0;`
            // after the buffer is allocated. LEN only becomes
            // `strlen+1` once a value is actually present.
            len: 0,
            olen: 0,
            simm: 0,
            siml: String::new(),
            siol: String::new(),
            sims: 0,
            mpst: 0,
            apst: 0,
            value_changed: false,
        }
    }
}

impl LsiRecord {
    pub fn new(val: &str) -> Self {
        let v = val.to_string();
        // LEN is `strlen+1` for a non-empty value; an empty initial
        // value leaves LEN=0 (C: no value present yet).
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

static LSI_FIELDS: &[FieldDesc] = &[
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
    // `menuPost` menu fields (DBF_MENU). Exposed as Short, matching the
    // record's other menu field (SIMM); C `lsiRecord.dbd.pod:107/113`.
    FieldDesc {
        name: "MPST",
        dbf_type: DbFieldType::Short,
        read_only: false,
    },
    FieldDesc {
        name: "APST",
        dbf_type: DbFieldType::Short,
        read_only: false,
    },
];

impl Record for LsiRecord {
    fn record_type(&self) -> &'static str {
        "lsi"
    }

    fn field_list(&self) -> &'static [FieldDesc] {
        LSI_FIELDS
    }

    fn long_string_fields(&self) -> &'static [&'static str] {
        &["VAL", "OVAL"]
    }

    fn uses_monitor_deadband(&self) -> bool {
        false
    }

    fn monitor_value_changed(&self) -> Option<bool> {
        Some(self.value_changed)
    }

    fn monitor_always_post(&self) -> (bool, bool) {
        // C `lsiRecord.c` monitor: `if (mpst == menuPost_Always) events |=
        // DBE_VALUE; if (apst == menuPost_Always) events |= DBE_LOG;`.
        (self.mpst == MENU_POST_ALWAYS, self.apst == MENU_POST_ALWAYS)
    }

    fn process(&mut self) -> CaResult<ProcessOutcome> {
        // C `lsiRecord.c::monitor` (lines 202-224) copies OVAL and
        // bumps OLEN *only when the value actually changed* —
        // `len != olen || memcmp(oval, val, len)` — and raises
        // `DBE_VALUE | DBE_LOG` on that same condition. `process()`
        // itself does not recompute LEN; LEN is set when VAL is written
        // (C `special`, Rust `put_field`). Recomputing it here would
        // make OLEN report the previous LEN after a no-op cycle.
        //
        // Capture the change BEFORE committing oval/olen, because the
        // framework's monitor gate reads `monitor_value_changed()`
        // *after* this returns — by then oval == val and olen == len.
        self.value_changed = self.len != self.olen || self.oval != self.val;
        if self.value_changed {
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
            "SIMM" => Some(EpicsValue::Short(self.simm)),
            "SIML" => Some(EpicsValue::String(self.siml.clone().into())),
            "SIOL" => Some(EpicsValue::String(self.siol.clone().into())),
            "SIMS" => Some(EpicsValue::Short(self.sims)),
            "MPST" => Some(EpicsValue::Short(self.mpst)),
            "APST" => Some(EpicsValue::Short(self.apst)),
            _ => None,
        }
    }

    fn put_field(&mut self, name: &str, value: EpicsValue) -> CaResult<()> {
        match name {
            "VAL" => {
                // A DBR_STRING-typed put (EpicsValue::String) is
                // itself capped at MAX_STRING_SIZE (40) by dbConvert
                // in C before it reaches the record — apply the same
                // cap. A DBR_CHAR long-string put (CharArray) is only
                // bounded by SIZV.
                let mut s = match value {
                    EpicsValue::String(s) => truncate_utf8(&s.as_str_lossy(), MAX_STRING_SIZE - 1),
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
                    // C `lsiRecord.c:46-55`: SIZV clamps to [16, 0x7fff].
                    self.sizv = (v as i32).clamp(16, 0x7fff) as u16;
                } else {
                    return Err(CaError::TypeMismatch("SIZV".into()));
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
            "MPST" => {
                if let EpicsValue::Short(v) = value {
                    self.mpst = v;
                } else {
                    return Err(CaError::TypeMismatch("MPST".into()));
                }
            }
            "APST" => {
                if let EpicsValue::Short(v) = value {
                    self.apst = v;
                } else {
                    return Err(CaError::TypeMismatch("APST".into()));
                }
            }
            _ => return Err(CaError::FieldNotFound(name.to_string())),
        }
        Ok(())
    }
}
