use crate::error::{CaError, CaResult};
use crate::server::record::{CycleCell, CycleRead};
use crate::server::record::{MENU_POST, MENU_YES_NO, ProcessOutcome, Record};
use crate::types::{EpicsValue, PvString};

/// EPICS `MAX_STRING_SIZE` — `val`/`oval`/`sval` are fixed 40-byte
/// buffers in C `stringinRecord.c`; every copy truncates at 40.
const MAX_STRING_SIZE: usize = 40;

/// `s` cut to at most `MAX_STRING_SIZE - 1` bytes. C
/// `strncpy(dst, src, sizeof(prec->val))` copies 39 payload bytes + an
/// implicit NUL byte for byte with no UTF-8 awareness, so the cut is on
/// a raw byte boundary and a non-UTF-8 VAL keeps its bytes verbatim.
fn truncated(s: &[u8]) -> &[u8] {
    &s[..s.len().min(MAX_STRING_SIZE - 1)]
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
    pub siml: PvString,
    pub siol: PvString,
    /// The SIOL read's buffer (`dbGetLink(&prec->siol, DBR_STRING,
    /// prec->sval)`, stringinRecord.c:208), copied to VAL on success.
    pub sval: PvString,
    pub sims: i16,
    pub sdly: f64,
    /// `menu(stringinPOST)` Post Value Monitors (0=On Change, 1=Always).
    pub mpst: i16,
    /// `menu(stringinPOST)` Post Archive Monitors (0=On Change, 1=Always).
    pub apst: i16,
}

/// `menu(stringinPOST)` — `stringinRecord.dbd.pod`: 0 = On Change, 1 = Always.
const MENU_POST_ALWAYS: i16 = 1;

impl Default for StringinRecord {
    fn default() -> Self {
        Self {
            val: PvString::new(),
            oval: PvString::new(),
            simm: 0,
            siml: PvString::new(),
            siol: PvString::new(),
            sval: PvString::new(),
            sims: 0,
            sdly: -1.0,
            mpst: 0,
            apst: 0,
        }
    }
}

impl StringinRecord {
    pub fn new(val: &str) -> Self {
        Self {
            val: PvString::from(truncated(val.as_bytes())),
            ..Default::default()
        }
    }
}

impl Record for StringinRecord {
    crate::slot_map!(crate::server::record::dbd_generated::slots::stringin;
        VAL => val,
        SVAL => sval,
        SDLY => sdly;
        read_only:
        OVAL => oval);

    fn acts_before_process(&self) -> bool {
        false
    }

    fn cycle_hooks(&self) -> crate::server::record::CycleHooks {
        crate::server::record::CycleHooks::NONE
    }

    fn record_type(&self) -> &'static str {
        "stringin"
    }

    /// devSiSoft.c returns `dbGetLink`'s status.
    fn process_status_rule(&self) -> crate::server::record::ProcessStatusRule {
        crate::server::record::ProcessStatusRule::io(crate::server::record::SoftStatus {
            plain: true,
            raw: false,
            async_: false,
        })
    }

    /// C reads INP (`devSiSoft.c:53`) and SIOL (`stringinRecord.c:208`) with
    /// a plain `dbGetLink(..., DBR_STRING, ...)`: an ENUM/MENU source
    /// delivers its state label, never the index digits (epics-base#183).
    fn input_link_request(&self, link_field: &str) -> crate::server::record::InputLinkRequest {
        crate::server::record::InputLinkRequest::As(match link_field {
            "INP" | "SIOL" => crate::server::record::LinkReadAs::String,
            _ => crate::server::record::LinkReadAs::Native,
        })
    }

    /// `devSiSoft.c:53` reads straight into VAL: the `putStringString` row
    /// `set_val` reaches for a String, without its field lookups.
    fn soft_input_read(
        &mut self,
        value: Option<EpicsValue>,
        _dpvt: &mut crate::server::device_support::Dpvt,
    ) -> CaResult<()> {
        match value {
            Some(EpicsValue::String(s)) => {
                self.val.set_bytes(truncated(s.as_bytes()));
                Ok(())
            }
            Some(value) => self.set_val(value),
            None => Ok(()),
        }
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

    /// C `stringinRecord.c:176-179` `monitor()`: the `strncmp(oval, val)` mismatch both
    /// raises `DBE_VALUE | DBE_LOG` and copies VAL into OVAL. Compared and
    /// committed HERE, at C's position, never captured during `process()`.
    fn monitor_value_changed(&mut self) -> Option<bool> {
        let changed = self.oval != self.val;
        if changed {
            self.oval.clone_from(&self.val);
        }
        Some(changed)
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

    /// `stringinRecord.c::process` likewise never re-derives UDF after a
    /// DEVICE read that wrote VAL directly (C `return 2`): its one
    /// `prec->udf = FALSE` is the simulated SIOL branch at `:211`. The dset
    /// owns the decision, and `devAsynOctet.c::callbackSiRead:918-932` shows
    /// why that matters — `readIt` stores into `psi->val` unconditionally but
    /// `psi->udf = 0` runs only inside `if (status == asynSuccess)`, so a
    /// partial read on a transport error leaves the record undefined with a
    /// truncated VAL. Re-deriving here would declare it defined.
    fn rederives_udf_on_computed_read(&self) -> bool {
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

    /// `monitor()` posts VAL alone (`stringinRecord.c:174-188`); OVAL is
    /// its private copy.
    fn process_posted_fields(&self) -> Option<crate::server::record::DeclaredFields> {
        Some(
            &const {
                crate::server::record::declared_set(
                    crate::server::record::dbd_generated::STRINGIN_FIELDS,
                    ["VAL"],
                )
            },
        )
    }

    fn process(&mut self) -> CaResult<ProcessOutcome> {
        Ok(ProcessOutcome::complete())
    }

    fn link_text_ref(&self, link_field: &str) -> Option<&[u8]> {
        match link_field {
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

    fn cycle_cell(&self, cell: CycleCell) -> CycleRead {
        match cell {
            CycleCell::Val | CycleCell::Oval => CycleRead::ByName,
            _ => CycleRead::Absent,
        }
    }

    fn val(&self) -> Option<EpicsValue> {
        Some(EpicsValue::String(self.val.clone()))
    }

    /// A string VAL is never undefined by value; the default asks [`Self::val`]
    /// for a copy to say so.
    fn value_is_undefined(&self) -> bool {
        false
    }

    fn has_sval(&self) -> bool {
        true
    }

    /// C `strcpy(prec->val, prec->sval)` (`stringinRecord.c:213`); SVAL is
    /// already cut at the C buffer size, so the copy is exact and allocates
    /// nothing once VAL has the capacity.
    fn land_sval(&mut self) -> CaResult<()> {
        self.val.clone_from(&self.sval);
        Ok(())
    }

    fn get_field(&self, name: &str) -> Option<EpicsValue> {
        match name {
            "VAL" => Some(EpicsValue::String(self.val.clone())),
            "OVAL" => Some(EpicsValue::String(self.oval.clone())),
            "SIMM" => Some(EpicsValue::Short(self.simm)),
            "SIML" => Some(EpicsValue::String(self.siml.clone().into())),
            "SIOL" => Some(EpicsValue::String(self.siol.clone().into())),
            "SVAL" => Some(EpicsValue::String(self.sval.clone())),
            "SIMS" => Some(EpicsValue::Short(self.sims)),
            "SDLY" => Some(EpicsValue::Double(self.sdly)),
            "MPST" => Some(EpicsValue::Short(self.mpst)),
            "APST" => Some(EpicsValue::Short(self.apst)),
            _ => None,
        }
    }

    // C `stringinRecord.c:117` copies VAL after `:112-115` returns a nonzero dset status.
    fn seed_deadband_tracking(&mut self, dset_init: crate::server::device_support::DsetInit) {
        if dset_init != crate::server::device_support::DsetInit::Failed {
            self.oval.clone_from(&self.val);
        }
    }

    fn put_field(&mut self, name: &str, value: EpicsValue) -> CaResult<()> {
        match name {
            "VAL" => match value {
                // C truncates every VAL copy at MAX_STRING_SIZE (40).
                EpicsValue::String(s) => {
                    self.val.set_bytes(truncated(s.as_bytes()));
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch("VAL".into())),
            },
            "OVAL" => match value {
                EpicsValue::String(s) => {
                    self.oval.set_bytes(truncated(s.as_bytes()));
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
            "SVAL" => match value {
                EpicsValue::String(s) => {
                    self.sval.set_bytes(truncated(s.as_bytes()));
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch("SVAL".into())),
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

    /// The soft read stores what `set_val` stores, at every length either
    /// side of the 39-byte cap and for raw bytes.
    #[test]
    fn soft_input_read_stores_what_set_val_stores() {
        let texts: [&[u8]; 6] = [
            b"",
            b"train",
            &[b'a'; 39],
            &[b'b'; 40],
            &[b'c'; 45],
            &[0xff, 0x00, 0x80],
        ];
        for text in texts {
            let value = EpicsValue::String(PvString::from_bytes(text.to_vec()));
            let mut read = StringinRecord::new("old");
            read.soft_input_read(Some(value.clone()), &mut Default::default())
                .unwrap();
            let mut put = StringinRecord::new("old");
            put.set_val(value).unwrap();
            assert_eq!(read.val, put.val, "{text:?}");
        }
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
        let inst = RecordInstance::new("SI:MPST", rec);
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
