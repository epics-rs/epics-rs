use crate::error::{CaError, CaResult};
use crate::server::record::{MENU_POST, MENU_YES_NO, ProcessOutcome, Record};
use crate::types::{EpicsValue, PvString};

/// EPICS `MAX_STRING_SIZE` — DBR_STRING buffers are 40 bytes.
const MAX_STRING_SIZE: usize = 40;

/// `menuPost_Always` — index 1 of the `menuPost` menu
/// (`menuPost.dbd.pod`: `choice(menuPost_OnChange, ...)` = 0,
/// `choice(menuPost_Always, ...)` = 1). MPST/APST default to
/// `menuPost_OnChange` (0).
const MENU_POST_ALWAYS: i16 = 1;

/// Truncate `s` to at most `max` bytes. C `lsiRecord.c` keeps the long
/// string in a fixed `char[]` and copies it byte for byte
/// (`memcpy`/`strncpy`) with no UTF-8 awareness, so the cut is on a raw
/// byte boundary and a non-UTF-8 value keeps its bytes verbatim.
fn truncate_bytes(s: PvString, max: usize) -> PvString {
    if s.len() <= max {
        return s;
    }
    PvString::from_bytes(s.as_bytes()[..max].to_vec())
}

// Long string input record (EPICS 7).
// Native CA type is DBR_CHAR array; SIZV (default 256) sets the max byte count.
// LEN reports the current string length (number of bytes including NUL terminator).
pub struct LsiRecord {
    pub val: PvString,
    pub oval: PvString,
    pub sizv: u16,
    pub len: u32,
    pub olen: u32,
    pub simm: i16,
    pub siml: String,
    pub siol: String,
    pub sims: i16,
    pub sdly: f64,
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
            val: PvString::new(),
            oval: PvString::new(),
            // Intentional deviation from C `lsiRecord.dbd.pod`
            // `field(SIZV,DBF_USHORT){ initial("41") }`: the port defaults to a
            // larger 256-byte long-string buffer rather than C's 41. Kept by
            // design (a .db that needs C's size sets SIZV explicitly); not a
            // parity bug.
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
            sdly: -1.0,
            mpst: 0,
            apst: 0,
            value_changed: false,
        }
    }
}

impl LsiRecord {
    pub fn new(val: &str) -> Self {
        let v = PvString::from(val);
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

    fn clamped(&self) -> PvString {
        let max = (self.sizv as usize).saturating_sub(1);
        truncate_bytes(self.val.clone(), max)
    }
}

impl Record for LsiRecord {
    fn record_type(&self) -> &'static str {
        "lsi"
    }

    /// C reads INP (`devLsiSoft.c:32`) and SIOL (`lsiRecord.c:244`) through
    /// `dbGetLinkLS` (`dbLink.c:497-505`), whose switch is on the SOURCE
    /// class: a `DBF_CHAR`/`DBF_UCHAR` source is read as the bytes it
    /// spells (capped at SIZV), anything else as `DBR_STRING` — so an
    /// ENUM/MENU source delivers its state label (epics-base#183).
    fn input_link_read_as(
        &self,
        link_field: &str,
        source: &crate::server::record::OutTarget,
    ) -> Option<crate::server::record::LinkReadAs> {
        use crate::server::record::LinkReadAs;
        use crate::types::DbFieldType;
        Some(match link_field {
            "INP" | "SIOL" => match source.field_type {
                Some(DbFieldType::Char | DbFieldType::UChar) => LinkReadAs::CharArrayAsString {
                    max_elements: self.sizv as usize,
                },
                _ => LinkReadAs::String,
            },
            _ => LinkReadAs::Native,
        })
    }

    /// `DBF_MENU` fields, served as `DBR_ENUM` (`lsiRecord.dbd.pod`): `SIMM`
    /// is `menu(menuYesNo)` (two-choice NO/YES). `MPST`/`APST` are
    /// `menu(menuPost)` (On Change, Always) — unlike the array records
    /// (`aai`/`aao`/`waveform`) whose POST menus reverse that order, so they
    /// are resolved here rather than globally. `SIMS`/`OLDSIMM` are shared
    /// menus resolved centrally.
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

    fn uses_monitor_deadband(&self) -> bool {
        false
    }

    /// `lsiRecord.c::process` has no unconditional UDF re-derive; UDF is
    /// cleared only where a value is actually loaded — the init-time
    /// `dbLoadLinkLS` (`lsiRecord.c:85-88`, applied by the init-seed owner)
    /// and the soft support's sourced read (`devLsiSoft.c`,
    /// `if (status == 0) prec->udf = FALSE`). A process cycle that sources
    /// nothing — e.g. a `caput .UDF 1` on a Passive record with a
    /// constant/empty INP — must keep the client's UDF put. Opt out of the
    /// per-cycle blanket re-derive, like `stringin`/`lso`.
    fn clears_udf(&self) -> bool {
        false
    }

    /// `lsiRecord.c` has NO `recGblCheckUdf` / `UDF_ALARM` (unlike
    /// `lsoRecord.c:118`): an undefined lsi raises no alarm from UDF (softIoc:
    /// `record(lsi,"X"){}` → UDF 1, STAT/SEVR = NO_ALARM). With `clears_udf`
    /// false, UDF can now legitimately stay 1, so this MUST be false or
    /// `rec_gbl_check_udf` would invent an alarm C never raises.
    fn raises_udf_alarm(&self) -> bool {
        false
    }

    /// C `devLsiSoft.c:24` — the soft input support's
    /// `dbLoadLinkLS(&prec->inp, prec->val, prec->sizv, &prec->len)`. The
    /// load runs in the init-seed owner, which gates it on the soft DTYP the
    /// way C gates it on which device support is bound.
    fn constant_ls_link(&self) -> Option<&'static str> {
        Some("INP")
    }

    /// The `dbLoadLinkLS` sink plus C's init tail (`lsiRecord.c:85-88`).
    fn apply_ls_load(&mut self, load: crate::server::record::LsLoad) -> u32 {
        match load {
            crate::server::record::LsLoad::Text(s) => {
                let max = (self.sizv as usize).saturating_sub(1);
                self.val = truncate_bytes(PvString::from(s), max);
                self.len = (self.val.len() + 1) as u32;
            }
            // C's number case: the buffer is untouched, LEN comes out 1.
            crate::server::record::LsLoad::LenOnly => self.len = 1,
        }
        self.oval = self.val.clone();
        self.olen = self.len;
        self.len
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
            "SIZV" => Some(EpicsValue::UShort(self.sizv)),
            "LEN" => Some(EpicsValue::ULong(self.len)),
            "OLEN" => Some(EpicsValue::ULong(self.olen)),
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
            "VAL" => {
                // A DBR_STRING-typed put (EpicsValue::String) is
                // itself capped at MAX_STRING_SIZE (40) by dbConvert
                // in C before it reaches the record — apply the same
                // cap. A DBR_CHAR long-string put (CharArray) is only
                // bounded by SIZV.
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
                // SIZV is DBF_USHORT (lsiRecord.dbd.pod:75): a client put
                // arrives as UShort, internal callers may still pass Short.
                let raw = match value {
                    EpicsValue::UShort(v) => v as i32,
                    EpicsValue::Short(v) => v as i32,
                    _ => return Err(CaError::TypeMismatch("SIZV".into())),
                };
                // C `lsiRecord.c:46-55`: SIZV clamps to [16, 0x7fff].
                self.sizv = raw.clamp(16, 0x7fff) as u16;
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
