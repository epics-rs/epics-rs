use super::lsi::LongStringVal;
use crate::error::{CaError, CaResult};
use crate::server::record::{FieldDeclaration, MENU_POST, MENU_YES_NO, ProcessOutcome, Record};
use crate::types::{EpicsValue, PvString};

/// EPICS `MAX_STRING_SIZE` — DBR_STRING buffers are 40 bytes.
const MAX_STRING_SIZE: usize = 40;

/// `menuPost_Always` — index 1 of the `menuPost` menu
/// (`menuPost.dbd.pod`: `choice(menuPost_OnChange, ...)` = 0,
/// `choice(menuPost_Always, ...)` = 1). MPST/APST default to
/// `menuPost_OnChange` (0).
const MENU_POST_ALWAYS: i16 = 1;

// Long string output record (EPICS 7).
// Native CA type is DBR_CHAR array; SIZV (default 256) sets the max byte count.
pub struct LsoRecord {
    pub val: LongStringVal,
    pub oval: LongStringVal,
    pub sizv: u16,
    pub ivoa: i16,
    pub ivov: PvString,
    pub omsl: i16,
    pub dol: PvString,
    pub simm: i16,
    pub siml: PvString,
    pub siol: PvString,
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
    /// LEN owed by `special()` on a VAL put, or by `monitor()`'s
    /// `len != olen` on the cycle it ran; `take_cycle_posted_fields` takes it.
    len_marked: bool,
}

impl Default for LsoRecord {
    fn default() -> Self {
        Self {
            val: LongStringVal::default(),
            oval: LongStringVal::default(),
            // Intentional deviation from C `lsoRecord.dbd.pod`
            // `field(SIZV,DBF_USHORT){ initial("41") }`: the port defaults to a
            // larger 256-byte long-string buffer rather than C's 41. Kept by
            // design (a .db that needs C's size sets SIZV explicitly); not a
            // parity bug.
            sizv: 256,
            ivoa: 0,
            ivov: PvString::new(),
            omsl: 0,
            dol: PvString::new(),
            simm: 0,
            siml: PvString::new(),
            siol: PvString::new(),
            sims: 0,
            sdly: -1.0,
            mpst: 0,
            apst: 0,
            len_marked: false,
        }
    }
}

impl LsoRecord {
    pub fn new(val: &str) -> Self {
        let mut rec = Self::default();
        if !val.is_empty() {
            rec.val = LongStringVal::from_text(&val.as_bytes()[..val.len().min(rec.capacity())]);
        }
        rec
    }

    /// VAL's byte capacity: SIZV less the NUL. VAL never exceeds it — every
    /// write, and a SIZV put, cuts to it.
    fn capacity(&self) -> usize {
        (self.sizv as usize).saturating_sub(1)
    }
}

impl Record for LsoRecord {
    // LEN/OLEN are VAL's and OVAL's lengths, not members.
    crate::slot_map!(crate::server::record::dbd_generated::slots::lso;
        fields:
        IVOV => ivov,
        SDLY => sdly,
        SIZV => sizv;
        custom: VAL, OVAL, LEN, OLEN, DOL, IVOA, OMSL, MPST, APST, SIML, SIMM, SIMS, SIOL;
        spill: OUT, OLDSIMM, SSCN, SIMPVT);

    fn acts_before_process(&self) -> bool {
        false
    }

    fn cycle_hooks(&self) -> crate::server::record::CycleHooks {
        crate::server::record::CycleHooks::WRITE_POSTED
    }

    fn record_type(&self) -> &'static str {
        "lso"
    }

    /// devLsoSoft.c and devLsoSoftCallback.c return `dbPutLinkLS`'s status.
    fn process_status_rule(&self) -> crate::server::record::ProcessStatusRule {
        crate::server::record::ProcessStatusRule::io(crate::server::record::SoftStatus {
            plain: true,
            raw: false,
            async_: true,
        })
        .ls()
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
    fn input_link_request(&self, link_field: &str) -> crate::server::record::InputLinkRequest {
        use crate::server::record::{InputLinkRequest, LinkReadAs};
        match link_field {
            "DOL" => InputLinkRequest::FromSource,
            _ => InputLinkRequest::As(LinkReadAs::Native),
        }
    }

    fn input_link_read_as_from_source(
        &self,
        _link_field: &str,
        source: &crate::server::record::OutTarget,
    ) -> Option<crate::server::record::LinkReadAs> {
        use crate::server::record::LinkReadAs;
        use crate::types::DbFieldType;
        Some(match source.field_type {
            Some(DbFieldType::Char | DbFieldType::UChar) => LinkReadAs::Native,
            _ => LinkReadAs::String,
        })
    }

    /// The closed-loop DOL read lands through `dbGetLinkLS`
    /// (`lsoRecord.c:114`), not the 40-byte DBR_STRING put of a VAL write.
    fn set_val(&mut self, value: EpicsValue) -> CaResult<()> {
        match super::lsi::link_ls_read(value, self.sizv, &mut self.val) {
            Ok(()) => Ok(()),
            Err(other) => self.put_field_internal("VAL", other),
        }
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

    /// C `cvt_dbaddr` serves each as one `DBF_STRING` (`no_elements = 1`,
    /// `lsoRecord.c:183`), whatever LEN holds.
    fn dbaddr_capacity(&self, field: &str) -> Option<u32> {
        self.field_is_long_string(field).then_some(1)
    }

    /// `cvt_dbaddr` stores `field_size = prec->sizv`.
    fn dbaddr_element_size(&self, field: &str) -> Option<u16> {
        self.field_is_long_string(field).then_some(self.sizv)
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
        self.val()
    }

    fn output_view(&self) -> Option<crate::server::record::OutView<'_>> {
        Some(crate::server::record::OutView::Chars(
            &self.val.elements()[..],
        ))
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
        let changed = self.oval != self.val;
        if changed {
            // Unchanged bytes have an unchanged length, and the take that
            // `marked` guarantees has already cleared the mark.
            self.len_marked = self.oval.copy_from(&self.val);
        }
        Some(changed)
    }

    /// C `lsoRecord.c:254-256`: `if (len != olen) { olen = len; db_post_events(&len,
    /// DBE_VALUE | DBE_LOG); }`, marked by `monitor_value_changed` as it
    /// commits OVAL, whose length is OLEN.
    fn monitor_value_classes(
        &mut self,
        _gate: crate::server::record::MonitorValueGate,
        _last_posted: &mut [f64; 2],
    ) -> crate::server::record::MonitorValueClasses {
        let changed = self.monitor_value_changed() == Some(true);
        let (val_always, archive_always) = self.monitor_always_post();
        crate::server::record::MonitorValueClasses {
            value: changed || val_always,
            archive: changed || archive_always,
            hash_changed: false,
            marked: self.len_marked,
        }
    }

    /// C `special()` sets `len` and posts it on every VAL put (lsoRecord.c:239),
    /// changed or not; `monitor()` posts it again only when `len != olen`.
    fn special(
        &mut self,
        field: &str,
        after: bool,
        _common: &crate::server::record::CommonFields,
    ) -> CaResult<()> {
        if after && field == "VAL" {
            self.len_marked = true;
        }
        Ok(())
    }

    fn take_cycle_posted_fields(&mut self) -> crate::server::record::CycleMarks {
        use crate::server::record::{CycleMarks, CyclePostMask};
        const LEN_POST_NAMED: [(&str, CyclePostMask); 1] = [("LEN", CyclePostMask::ValueLog)];
        static LEN_POST: [(
            crate::server::record::FieldIx,
            crate::server::record::CyclePostMask,
        ); 1] = crate::server::record::declared_post_table(
            crate::server::record::dbd_generated::LSO_FIELDS,
            LEN_POST_NAMED,
        );
        CycleMarks::of(&LEN_POST, std::mem::take(&mut self.len_marked) as u128)
    }

    /// LEN posts from `special()` and from `monitor()`'s `olen` compare, never
    /// against what a subscriber was last sent.
    fn fields_posted_only_when_marked(&self) -> &'static [&'static str] {
        &["LEN"]
    }

    /// C `monitor()` posts VAL and LEN alone (lsoRecord.c:246-265); the
    /// OVAL/OLEN copy it makes is never posted.
    fn process_posted_fields(&self) -> Option<crate::server::record::DeclaredFields> {
        Some(
            &const {
                crate::server::record::declared_set(
                    crate::server::record::dbd_generated::LSO_FIELDS,
                    ["VAL", "LEN"],
                )
            },
        )
    }

    fn stamps_after_write(&self) -> bool {
        true
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

    /// The `dbLoadLinkLS` sink (`lsoRecord.c:82`). The OVAL copy is not part
    /// of it: C makes that copy after the dset status return.
    fn apply_ls_load(&mut self, load: crate::server::record::LsLoad) -> u32 {
        match load {
            crate::server::record::LsLoad::Text(s) => {
                self.val
                    .set_text(&s.as_bytes()[..s.len().min(self.capacity())]);
            }
            crate::server::record::LsLoad::LenOnly => self.val.set_len_one(),
        }
        self.val.len()
    }

    /// C `lsoRecord.c:91-94`, after `:84-88` returns a nonzero dset status.
    fn seed_deadband_tracking(&mut self, dset_init: crate::server::device_support::DsetInit) {
        if dset_init != crate::server::device_support::DsetInit::Failed && !self.val.is_empty() {
            self.oval.copy_from(&self.val);
        }
    }

    fn process(&mut self) -> CaResult<ProcessOutcome> {
        Ok(ProcessOutcome::complete())
    }

    fn val(&self) -> Option<EpicsValue> {
        Some(EpicsValue::CharArray(self.val.elements().clone()))
    }

    /// [`Self::val`]'s elements, borrowed.
    fn monitor_value_view(&self) -> Option<crate::server::record::ValueView<'_>> {
        Some(crate::server::record::ValueView::Char(self.val.elements()))
    }

    fn link_text_ref(&self, link_field: &str) -> Option<&[u8]> {
        match link_field {
            "DOL" => Some(self.dol.as_bytes()),
            "SIML" => Some(self.siml.as_bytes()),
            "SIOL" => Some(self.siol.as_bytes()),
            _ => None,
        }
    }

    fn simm_cells(&self) -> Option<crate::server::record::SimmCells> {
        Some(crate::server::record::SimmCells {
            simm: self.simm,
            sims: self.sims,
            sdly: self.sdly,
        })
    }

    fn get_field(&self, name: &str) -> Option<EpicsValue> {
        match name {
            "VAL" => self.val(),
            "OVAL" => Some(EpicsValue::CharArray(self.oval.elements().clone())),
            "SIZV" => Some(EpicsValue::UShort(self.sizv)),
            "LEN" => Some(EpicsValue::ULong(self.val.len())),
            "OLEN" => Some(EpicsValue::ULong(self.oval.len())),
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

    /// C `lsoRecord.c:51-58`: SIZV clamps to [16, 0x7fff] and sizes VAL.
    fn init_record(&mut self, pass: u8) -> CaResult<()> {
        if pass == 0 {
            self.sizv = self.sizv.clamp(16, 0x7fff);
            // C callocs VAL and OVAL here, with `len = olen = 0`.
            self.val = LongStringVal::default();
            self.oval = LongStringVal::default();
        }
        Ok(())
    }

    /// LEN and OLEN are the lengths of VAL and OVAL; `put_field` refuses the
    /// `SPC_NOMOD` fields, and a `.db` value is stored as C stores it.
    fn load_field(&mut self, name: &str, value: EpicsValue) -> CaResult<()> {
        match (name, value) {
            ("LEN", EpicsValue::ULong(n)) => self.val.set_len(n),
            ("OLEN", EpicsValue::ULong(n)) => self.oval.set_len(n),
            (name, value) => return self.put_field(name, value),
        }
        Ok(())
    }

    fn put_field(&mut self, name: &str, value: EpicsValue) -> CaResult<()> {
        match name {
            "VAL" => {
                let capacity = self.capacity();
                self.val.put(value, capacity)?;
            }
            "SIZV" => {
                // SIZV is DBF_USHORT (lsoRecord.dbd.pod:128): a client put
                // arrives as UShort, internal callers may still pass Short.
                self.sizv = match value {
                    EpicsValue::UShort(v) => v,
                    EpicsValue::Short(v) => v as u16,
                    _ => return Err(CaError::TypeMismatch("SIZV".into())),
                };
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
                let bytes = match &value {
                    EpicsValue::String(s) => s.as_bytes(),
                    EpicsValue::CharArray(bytes) => {
                        let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
                        &bytes[..end]
                    }
                    _ => return Err(CaError::TypeMismatch("IVOV".into())),
                };
                self.ivov
                    .set_bytes(&bytes[..bytes.len().min(MAX_STRING_SIZE - 1)]);
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
