use crate::error::{CaError, CaResult};
use crate::server::record::{FieldDesc, MENU_POST, MENU_YES_NO, ProcessOutcome, Record};
use crate::types::{DbFieldType, EpicsValue, PvString};

/// EPICS `MAX_STRING_SIZE` — DBR_STRING buffers are 40 bytes.
const MAX_STRING_SIZE: usize = 40;

/// `menuPost_Always` — index 1 of the `menuPost` menu
/// (`menuPost.dbd.pod`: `choice(menuPost_OnChange, ...)` = 0,
/// `choice(menuPost_Always, ...)` = 1). MPST/APST default to
/// `menuPost_OnChange` (0).
const MENU_POST_ALWAYS: i16 = 1;

/// Truncate `s` to at most `max` bytes. C `lsoRecord.c` keeps the long
/// string in a fixed `char[]` and copies it byte for byte
/// (`memcpy`/`strncpy`) with no UTF-8 awareness, so the cut is on a raw
/// byte boundary and a non-UTF-8 value keeps its bytes verbatim.
fn truncate_bytes(s: PvString, max: usize) -> PvString {
    if s.len() <= max {
        return s;
    }
    PvString::from_bytes(s.as_bytes()[..max].to_vec())
}

// Long string output record (EPICS 7).
// Native CA type is DBR_CHAR array; SIZV (default 256) sets the max byte count.
pub struct LsoRecord {
    pub val: PvString,
    pub oval: PvString,
    pub sizv: u16,
    pub len: u32,
    pub olen: u32,
    pub ivoa: i16,
    pub ivov: PvString,
    pub omsl: i16,
    pub dol: String,
    pub simm: i16,
    pub siml: String,
    pub siol: String,
    pub sims: i16,
    /// `menuPost` Post Value Monitors: `menuPost_OnChange` (0, default)
    /// posts DBE_VALUE only on a real change; `menuPost_Always` (1) posts
    /// DBE_VALUE every write (C `lsoRecord.dbd.pod` MPST, monitor:
    /// `if (mpst == menuPost_Always) events |= DBE_VALUE;`).
    pub mpst: i16,
    /// `menuPost` Post Archive Monitors: same as [`Self::mpst`] for the
    /// DBE_LOG (archive) mask (C `lsoRecord.dbd.pod` APST, monitor:
    /// `if (apst == menuPost_Always) events |= DBE_LOG;`).
    pub apst: i16,
    /// Per-cycle scratch: did VAL change on the most recent `process()`?
    /// Captured BEFORE `process()` commits `oval`/`olen` so the
    /// framework's post-process monitor gate can see it. C
    /// `lsoRecord.c::monitor`: `len != olen || memcmp(oval, val, len)`.
    value_changed: bool,
}

impl Default for LsoRecord {
    fn default() -> Self {
        Self {
            val: PvString::new(),
            oval: PvString::new(),
            sizv: 256,
            // C `lsoRecord.c:62-64`: `prec->len = 0; prec->olen = 0;`.
            len: 0,
            olen: 0,
            ivoa: 0,
            ivov: PvString::new(),
            omsl: 0,
            dol: String::new(),
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

impl LsoRecord {
    pub fn new(val: &str) -> Self {
        let v = PvString::from(val);
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

    fn clamped(&self) -> PvString {
        let max = (self.sizv as usize).saturating_sub(1);
        truncate_bytes(self.val.clone(), max)
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
        // C declares SIZV as DBF_USHORT (lsoRecord.dbd.pod:128): the VAL buffer
        // size is an unsigned 16-bit count (clamped to [16, 0x7fff] at init).
        dbf_type: DbFieldType::UShort,
        read_only: false,
    },
    FieldDesc {
        name: "LEN",
        // C declares LEN as DBF_ULONG (lsoRecord.dbd.pod:135): the current
        // string byte length is an unsigned 32-bit count.
        dbf_type: DbFieldType::ULong,
        read_only: true,
    },
    FieldDesc {
        name: "OLEN",
        // C declares OLEN as DBF_ULONG (lsoRecord.dbd.pod:139): the previously
        // posted byte length is an unsigned 32-bit count.
        dbf_type: DbFieldType::ULong,
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
    // `menuPost` menu fields (DBF_MENU). Exposed as Short, matching the
    // record's other menu field (SIMM); C `lsoRecord.dbd.pod:172/178`.
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

impl Record for LsoRecord {
    fn record_type(&self) -> &'static str {
        "lso"
    }

    fn field_list(&self) -> &'static [FieldDesc] {
        LSO_FIELDS
    }

    /// `DBF_MENU` fields, served as `DBR_ENUM` (`lsoRecord.dbd.pod`): `SIMM`
    /// is `menu(menuYesNo)` (two-choice NO/YES). `MPST`/`APST` are
    /// `menu(menuPost)` (On Change, Always) — unlike the array records whose
    /// POST menus reverse that order, so they are resolved here rather than
    /// globally. `SIMS`/`OLDSIMM`/`OMSL`/`IVOA` are shared menus resolved
    /// centrally.
    fn menu_field_choices(&self, field: &str) -> Option<&'static [&'static str]> {
        match field {
            "SIMM" => Some(MENU_YES_NO),
            "MPST" | "APST" => Some(MENU_POST),
            _ => None,
        }
    }

    fn long_string_fields(&self) -> &'static [&'static str] {
        &["VAL", "OVAL"]
    }

    // C recLso.c IVOA=set_to_IVOV: oval = ivov (string copy); val = oval.
    fn apply_invalid_output_value(&mut self, ivov: EpicsValue) -> CaResult<()> {
        self.put_field("OVAL", ivov.clone())?;
        self.put_field("VAL", ivov)
    }

    fn uses_monitor_deadband(&self) -> bool {
        false
    }

    fn monitor_value_changed(&self) -> Option<bool> {
        Some(self.value_changed)
    }

    fn monitor_always_post(&self) -> (bool, bool) {
        // C `lsoRecord.c` monitor: `if (mpst == menuPost_Always) events |=
        // DBE_VALUE; if (apst == menuPost_Always) events |= DBE_LOG;`.
        (self.mpst == MENU_POST_ALWAYS, self.apst == MENU_POST_ALWAYS)
    }

    /// C `lsoRecord.c::init_record` loads a constant DOL into VAL once via
    /// `dbLoadLinkLS(&prec->dol, prec->val, prec->sizv, &prec->len)`. The
    /// framework gate (`processing.rs`) excludes a constant DOL from the
    /// per-cycle closed-loop fetch (C `!dbLinkIsConstant`), so the constant
    /// text must be seeded here. A quoted (`"text"`) or bare-numeric (`5`)
    /// DOL parses to `ParsedLink::Constant`; its text is copied into the
    /// long-string VAL and LEN is set to the C `strlen+1` convention.
    fn init_record(&mut self, pass: u8) -> CaResult<()> {
        if pass == 0 {
            if let crate::server::record::ParsedLink::Constant(s) =
                crate::server::record::parse_link_v2(&self.dol)
            {
                self.val = PvString::from(s);
                self.len = if self.val.is_empty() {
                    0
                } else {
                    (self.val.len() + 1).min(256) as u32
                };
            }
        }
        Ok(())
    }

    fn process(&mut self) -> CaResult<ProcessOutcome> {
        // C `lsoRecord.c::monitor` (lines 244-256): copy OVAL and bump
        // OLEN only when the value actually changed, and raise
        // `DBE_VALUE | DBE_LOG` on that same condition. LEN is set when
        // VAL is written (C `special`, Rust `put_field`); `process()`
        // must not recompute it.
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
            "SIZV" => Some(EpicsValue::UShort(self.sizv)),
            "LEN" => Some(EpicsValue::ULong(self.len)),
            "OLEN" => Some(EpicsValue::ULong(self.olen)),
            "IVOA" => Some(EpicsValue::Short(self.ivoa)),
            "IVOV" => Some(EpicsValue::CharArray(self.ivov.clone().into_bytes())),
            "OMSL" => Some(EpicsValue::Short(self.omsl)),
            "DOL" => Some(EpicsValue::String(self.dol.clone().into())),
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
                // DBR_STRING-typed put caps at MAX_STRING_SIZE (40);
                // DBR_CHAR long-string put is bounded only by SIZV.
                let mut s = match value {
                    EpicsValue::String(s) => truncate_bytes(s, MAX_STRING_SIZE - 1),
                    EpicsValue::CharArray(bytes) => {
                        let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
                        PvString::from_bytes(bytes[..end].to_vec())
                    }
                    _ => return Err(CaError::TypeMismatch("VAL".into())),
                };
                let max = (self.sizv as usize).saturating_sub(1);
                if s.len() > max {
                    s = truncate_bytes(s, max);
                }
                self.val = s;
                self.len = (self.val.len() + 1) as u32;
            }
            "SIZV" => {
                // SIZV is DBF_USHORT (lsoRecord.dbd.pod:128): a client put
                // arrives as UShort, internal callers may still pass Short.
                let raw = match value {
                    EpicsValue::UShort(v) => v as i32,
                    EpicsValue::Short(v) => v as i32,
                    _ => return Err(CaError::TypeMismatch("SIZV".into())),
                };
                // C `lsoRecord.c:51-58`: SIZV clamps to [16, 0x7fff].
                self.sizv = raw.clamp(16, 0x7fff) as u16;
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
                    self.ivov = PvString::from_bytes(bytes[..end].to_vec());
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
