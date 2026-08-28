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
    pub sdly: f64,
    /// `menuPost` Post Value Monitors: `menuPost_OnChange` (0, default)
    /// posts DBE_VALUE only on a real change; `menuPost_Always` (1) posts
    /// DBE_VALUE every write (C `lsoRecord.dbd.pod` MPST, monitor:
    /// `if (mpst == menuPost_Always) events |= DBE_VALUE;`).
    pub mpst: i16,
    /// `menuPost` Post Archive Monitors: same as [`Self::mpst`] for the
    /// DBE_LOG (archive) mask (C `lsoRecord.dbd.pod` APST, monitor:
    /// `if (apst == menuPost_Always) events |= DBE_LOG;`).
    pub apst: i16,
}

impl Default for LsoRecord {
    fn default() -> Self {
        Self {
            val: PvString::new(),
            oval: PvString::new(),
            // Intentional deviation from C `lsoRecord.dbd.pod`
            // `field(SIZV,DBF_USHORT){ initial("41") }`: the port defaults to a
            // larger 256-byte long-string buffer rather than C's 41. Kept by
            // design (a .db that needs C's size sets SIZV explicitly); not a
            // parity bug.
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
            sdly: -1.0,
            mpst: 0,
            apst: 0,
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

impl Record for LsoRecord {
    fn record_type(&self) -> &'static str {
        "lso"
    }

    /// C `lsoRecord.c:108-122`: the scalar `dbGetLink(&prec->dol, ..., &prec->val, 0, 0)`
    /// under `dol.type != CONSTANT && omsl == menuOmslclosed_loop`.
    fn fetches_dol_closed_loop(&self) -> bool {
        true
    }

    /// C reads the closed-loop DOL through `dbGetLinkLS` (`lsoRecord.c:114`,
    /// `dbLink.c:497-505`), whose switch is on the SOURCE class: a
    /// `DBF_CHAR`/`DBF_UCHAR` source is read as the bytes it spells (capped
    /// at SIZV), anything else as `DBR_STRING` — so an ENUM/MENU source
    /// delivers its state label (epics-base#183).
    fn input_link_read_as(
        &self,
        link_field: &str,
        source: &crate::server::record::OutTarget,
    ) -> Option<crate::server::record::LinkReadAs> {
        use crate::server::record::LinkReadAs;
        use crate::types::DbFieldType;
        Some(match link_field {
            "DOL" => match source.field_type {
                Some(DbFieldType::Char | DbFieldType::UChar) => LinkReadAs::CharArrayAsString {
                    max_elements: self.sizv as usize,
                },
                _ => LinkReadAs::String,
            },
            _ => LinkReadAs::Native,
        })
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

    /// C `lsoRecord.c::process:131-140` (`menuIvoaSet_output_to_IVOV`) writes
    /// the record's OWN value and nothing else: `strncpy(prec->val,
    /// prec->ivov, prec->sizv - 1)`, terminate, `prec->len = strlen(prec->val)
    /// + 1`. `put_field("VAL", ..)` performs exactly that pair of steps.
    ///
    /// `oval` is deliberately NOT written here. C assigns it at two places in
    /// the whole record — `:92` (`strcpy(prec->oval, prec->val)` in the init
    /// tail) and `:251` (inside `monitor()`) — and both copy it FROM `val`: it
    /// is `monitor()`'s previous-value tracker, and the comparison
    /// `len != olen || memcmp(oval, val, len)` (`:248-249`) is what posts
    /// DBE_VALUE|DBE_LOG for this write. Seeding `oval` with the value about
    /// to be stored makes that comparison equal forever and suppresses the
    /// post for the life of the record.
    fn apply_invalid_output_value(&mut self, ivov: EpicsValue) -> CaResult<()> {
        self.put_field("VAL", ivov)
    }

    /// SINGLE source of the value both of lso's output paths carry: the real
    /// write `devLsoSoft.c::write_string:21` (`dbPutLinkLS(&prec->out,
    /// prec->val, prec->len)`) and the SIMM redirect `lsoRecord.c:288`
    /// (`dbPutLinkLS(&prec->siol, prec->val, prec->len)`). Both put `val`.
    ///
    /// The trait default is `OVAL.or(VAL)` — the ao/calcout convention, where
    /// OVAL is the record's computed output. lso has no computed-output field;
    /// its OVAL is the monitor tracker above, holding the value published on
    /// the PREVIOUS cycle. Naming VAL here is what lets the IVOA arm leave
    /// OVAL to its one meaning.
    fn output_link_value(&self) -> Option<EpicsValue> {
        Some(EpicsValue::CharArray(self.clamped().into_bytes()))
    }

    fn uses_monitor_deadband(&self) -> bool {
        false
    }

    /// C `lsoRecord.c:248-252` `monitor()`: `if (prec->len != prec->olen ||
    /// memcmp(prec->oval, prec->val, prec->len)) { events |= DBE_VALUE |
    /// DBE_LOG; memcpy(prec->oval, prec->val, prec->len); }` — compared and
    /// committed HERE, at C's position. (`olen` follows from the separate
    /// `if (prec->len != prec->olen)` two lines down, which also posts LEN.)
    ///
    /// For `lso` this is what makes the IVOA = Set_output_to_IVOV write post
    /// on its OWN cycle: the framework's IVOA owner runs after
    /// `process()` returns, so a flag captured in `process()` would be the
    /// verdict on the PRE-arm value.
    fn monitor_value_changed(&mut self) -> Option<bool> {
        let changed = self.len != self.olen || self.oval != self.val;
        if changed {
            self.oval = self.val.clone();
            self.olen = self.len;
        }
        Some(changed)
    }

    /// C `lsoRecord.c:113-115`: `process()` clears `udf` to FALSE only on a
    /// successful closed-loop DOL fetch (`if (!dbGetLinkLS(...)) prec->udf =
    /// FALSE;`); with no closed-loop fetch this cycle, `udf` is left
    /// untouched and `if (prec->udf) recGblSetSevr(UDF_ALARM, ...)` (`:117-118`)
    /// raises it every cycle for a bare record. `udf` is never re-derived
    /// from the stored VAL, so lso opts out of the framework's blanket
    /// per-cycle clear. The definers are a direct VAL put (`dbPut`,
    /// `field_io.rs`) and the DOL-apply site (`processing.rs`).
    fn clears_udf(&self) -> bool {
        false
    }

    /// C `lsoRecord.c:117-118` raises UDF with the literal `INVALID_ALARM`, not
    /// `prec->udfs`:
    ///
    /// ```c
    /// if (prec->udf)
    ///     recGblSetSevr(prec, UDF_ALARM, INVALID_ALARM);
    /// ```
    ///
    /// so `UDFS` cannot weaken or silence this record's UDF alarm. `stringout`, the same shape of output record, passes `prec->udfs`
    /// (`stringoutRecord.c:147`), so this is not a long-string or an output rule.
    fn udf_alarm_severity(&self) -> Option<crate::server::record::AlarmSeverity> {
        Some(crate::server::record::AlarmSeverity::Invalid)
    }

    fn monitor_always_post(&self) -> (bool, bool) {
        // C `lsoRecord.c` monitor: `if (mpst == menuPost_Always) events |=
        // DBE_VALUE; if (apst == menuPost_Always) events |= DBE_LOG;`.
        (self.mpst == MENU_POST_ALWAYS, self.apst == MENU_POST_ALWAYS)
    }

    /// C `lsoRecord.c:82` — `dbLoadLinkLS(&prec->dol, prec->val, prec->sizv,
    /// &prec->len)`. The load itself is the init-seed owner's
    /// (`seed_constant_links`); this only names the link it reads.
    fn constant_ls_link(&self) -> Option<&'static str> {
        Some("DOL")
    }

    /// The `dbLoadLinkLS` sink plus C's init tail (`lsoRecord.c:92-95`).
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

    fn process(&mut self) -> CaResult<ProcessOutcome> {
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
            // `DBF_STRING size(40)` (`lsoRecord.dbd.pod:155-160`), the same
            // plain 40-byte field `stringout` has — not a long string. VAL and
            // OVAL above are the SIZV-sized buffers `lsoRecord.c:63` allocates;
            // IVOV is `char ivov[40]` and must report the type it is declared,
            // because `apply_fields` parses a `.db` value as the type the
            // record stores.
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
            // C `putStringString` (`dbConvert.c:923-926`) copies at most
            // `field_size` bytes into a DBF_STRING and NUL-terminates the last
            // one, so IVOV's 40-byte field holds 39 bytes of text however it is
            // written.
            "IVOV" => {
                let s = match value {
                    EpicsValue::String(s) => s,
                    EpicsValue::CharArray(bytes) => {
                        let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
                        PvString::from_bytes(bytes[..end].to_vec())
                    }
                    _ => return Err(CaError::TypeMismatch("IVOV".into())),
                };
                self.ivov = truncate_bytes(s, MAX_STRING_SIZE - 1);
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
