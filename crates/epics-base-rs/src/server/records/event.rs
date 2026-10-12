use crate::error::{CaError, CaResult};
use crate::server::record::{CycleCell, CycleRead};
use crate::server::record::{MENU_YES_NO, Record};
use crate::types::{EpicsValue, PvString};
use std::sync::Arc;

/// `eventRecord.dbd` `field(VAL,DBF_STRING) size(40)`.
const MAX_STRING_SIZE: usize = 40;

/// `event` record — software-event source.
///
/// C parity (`eventRecord.dbd.pod:71` `field(VAL,DBF_STRING)`,
/// `eventRecord.c:107-132`): `VAL` is the *event name* string. On
/// every `process()` the record posts that named event via
/// `postEvent(eventNameToHandle(prec->val))`, waking every
/// `SCAN="Event"` record whose `EVNT` matches. Posting the event is
/// the record's entire purpose.
///
/// The framework (`processing.rs`) reads `VAL` after `process()` and
/// routes the event through [`crate::server::database::PvDatabase::post_event_named`] — the
/// record stays a pure state machine with no direct DB access.
///
/// Manually implements [`Record`] rather than using `#[derive(EpicsRecord)]`
/// so it can declare [`Record::fields_posted_with_monitor_mask`] — the derive
/// emits only the four mandatory methods (the same reason `longout` is hand
/// written).
pub struct EventRecord {
    /// Event name to post. DBF_STRING in C (was a numeric subscript
    /// pre-EPICS-7; a numeric string still works for back-compat). Bytes, as
    /// C's `char[40]` is. Written only by [`Self::set_val_text`].
    val: PvString,
    /// C `epvt`: [`EventHandle::of`] VAL, settled with every VAL write.
    epvt: Option<EventHandle>,
    /// SIMM is `DBF_MENU menu(menuYesNo)` (eventRecord.dbd.pod:152-156):
    /// the two-choice NO/YES simulation menu, served as DBR_ENUM.
    pub simm: i16,
    pub siml: String,
    pub siol: String,
    /// SVAL is `DBF_STRING` (eventRecord.dbd.pod:143-145) — the BUFFER C's
    /// `readValue` reads SIOL into (`dbGetLink(&prec->siol, DBR_STRING,
    /// &prec->sval)`, eventRecord.c:185) before publishing `val = sval`.
    pub sval: PvString,
    pub sims: i16,
    /// SDLY — "Sim. Mode Async Delay" (`DBF_DOUBLE`, `initial("-1.0")`,
    /// eventRecord.dbd.pod:171-177). A non-negative SDLY makes the simulated
    /// SIOL read asynchronous: C's `readValue` arms
    /// `callbackRequestProcessCallbackDelayed(..., prec->sdly)` and holds PACT
    /// across the delay (eventRecord.c:184-201). The framework reads the delay
    /// via `resolve_field("SDLY")`, which falls back to the dbd `initial`, so
    /// this field is the record's own store for it rather than a requirement.
    pub sdly: f64,
}

impl Default for EventRecord {
    fn default() -> Self {
        Self {
            val: PvString::new(),
            epvt: None,
            simm: 0,
            siml: String::new(),
            siol: String::new(),
            sval: PvString::new(),
            sims: 0,
            // C `field(SDLY,DBF_DOUBLE) { initial("-1.0") }` — negative means
            // "synchronous simulation".
            sdly: -1.0,
        }
    }
}

impl EventRecord {
    /// Construct an event record that posts event name `val`.
    pub fn new(val: impl Into<String>) -> Self {
        let mut rec = Self::default();
        rec.set_val_text(PvString::from(val.into()));
        rec
    }

    /// C `special(VAL)` and `devEventSoft` `read_event`: VAL and its handle
    /// move together, so the cycle posts without re-reading the name.
    fn set_val_text(&mut self, val: PvString) {
        self.epvt = EventHandle::of(&val);
        self.val = val;
    }
}

/// C `EVENTPVT` (`eventNameToHandle`, `dbScan.c:469`): the event a name
/// posts, `None` for the names C maps to no event (empty, `"0"`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EventHandle(Arc<str>);

impl EventHandle {
    fn of(name: &PvString) -> Option<Self> {
        crate::server::database::scan_index::EventName::of(&name.as_str_lossy())
            .map(|event| Self(event.to_string().into()))
    }

    /// The normalised name `post_event_named` routes by.
    pub fn name(&self) -> &str {
        &self.0
    }
}

fn put_pv_string(slot: &mut PvString, value: EpicsValue, field: &str) -> CaResult<()> {
    match value {
        EpicsValue::String(v) => {
            *slot = v;
            Ok(())
        }
        _ => Err(CaError::TypeMismatch(field.to_string())),
    }
}

fn put_string(slot: &mut String, value: EpicsValue, field: &str) -> CaResult<()> {
    match value {
        EpicsValue::String(v) => {
            *slot = v.as_str_lossy().into_owned();
            Ok(())
        }
        _ => Err(CaError::TypeMismatch(field.to_string())),
    }
}

fn put_short(slot: &mut i16, value: EpicsValue, field: &str) -> CaResult<()> {
    match value {
        EpicsValue::Short(v) => {
            *slot = v;
            Ok(())
        }
        _ => Err(CaError::TypeMismatch(field.to_string())),
    }
}

fn put_double(slot: &mut f64, value: EpicsValue, field: &str) -> CaResult<()> {
    match value {
        EpicsValue::Double(v) => {
            *slot = v;
            Ok(())
        }
        _ => Err(CaError::TypeMismatch(field.to_string())),
    }
}

impl Record for EventRecord {
    fn acts_before_process(&self) -> bool {
        false
    }

    fn cycle_hooks(&self) -> crate::server::record::CycleHooks {
        crate::server::record::CycleHooks::NONE
    }

    /// `monitor()` posts VAL alone (`eventRecord.c:157-165`).
    fn process_posted_fields(&self) -> Option<crate::server::record::DeclaredFields> {
        Some(
            &const {
                crate::server::record::declared_set(
                    crate::server::record::dbd_generated::EVENT_FIELDS,
                    ["VAL"],
                )
            },
        )
    }

    fn has_array_monitor_post(&self) -> bool {
        false
    }

    fn record_type(&self) -> &'static str {
        "event"
    }

    /// `devEventSoft.c:56` reads INP as `DBR_STRING`; C ships no other soft
    /// event dset.
    fn soft_input_request(
        &self,
        _soft: crate::server::device_support::SoftDtyp,
    ) -> crate::server::record::InputLinkRequest {
        crate::server::record::InputLinkRequest::As(crate::server::record::LinkReadAs::String)
    }

    /// C `eventRecord.c::process` never clears UDF: the record's one
    /// `prec->udf = FALSE` is inside `readValue`'s simulated SIOL branch
    /// (`:191`), which the framework's simulation tail already reproduces. A
    /// plain event record therefore keeps the UDF it was loaded with — softIoc:
    /// `record(event,"E2"){}` processes to UDF=1, while `field(VAL,"1")` in the
    /// `.db` defines it at load (UDF=0).
    fn clears_udf(&self) -> bool {
        false
    }

    /// The same fact governs a DEVICE read that wrote VAL directly (C
    /// `return 2`): `eventRecord.c::process` has no `prec->udf` assignment
    /// outside the `:191` simulation branch, so the dset's own write is final
    /// and the framework must not re-derive on top of it.
    fn rederives_udf_on_computed_read(&self) -> bool {
        false
    }

    /// `eventRecord.c` has NO `checkAlarms` either — an event record with UDF=1
    /// publishes NO_ALARM (softIoc: `record(event,"E2"){}` → UDF 1, SEVR
    /// NO_ALARM, even with the default `UDFS = INVALID`).
    fn raises_udf_alarm(&self) -> bool {
        false
    }

    /// C `eventRecord.c::monitor` (157-165) is the whole of the record's
    /// posting:
    ///
    /// ```c
    /// monitor_mask = recGblResetAlarms(prec);
    /// db_post_events(prec,&prec->val,monitor_mask|DBE_VALUE);
    /// ```
    ///
    /// `monitor_mask` is `recGblResetAlarms`'s return — the alarm bits alone —
    /// so VAL's post carries `DBE_VALUE` (plus `DBE_ALARM` on a cycle whose
    /// alarm moved) and NEVER `DBE_LOG`. event has no MDEL/ADEL, and the post
    /// is not guarded by `if (monitor_mask)`, so it fires on every process
    /// whether or not the event name changed.
    ///
    /// The port's generic path gave VAL the framework default
    /// `DBE_VALUE | DBE_LOG`, so a `DBE_LOG`-only archiver was sent the event
    /// name on every process — updates C sends it on no cycle at all. Naming
    /// VAL here routes it through the LOG-stripping arm of
    /// `RecordInstance::deadband_post`, the single owner of that mask.
    fn fields_posted_with_monitor_mask(&self) -> &'static [&'static str] {
        &["VAL"]
    }

    /// event has no MDEL: the deadband parsed a numeric name ("1") and posted
    /// VAL only when it moved, where C's post above is unconditional.
    fn uses_monitor_deadband(&self) -> bool {
        false
    }

    /// The default's answer for a string VAL, without cloning it every cycle:
    /// no event name is undefined (`devEventSoft.c:84` sets `udf = FALSE`).
    fn value_is_undefined(&self) -> bool {
        false
    }

    /// `SIMM` is `menu(menuYesNo)`, served as DBR_ENUM with the NO/YES labels.
    fn menu_field_choices(&self, field: &str) -> Option<&'static [&'static str]> {
        match field {
            "SIMM" => Some(MENU_YES_NO),
            _ => None,
        }
    }

    fn link_text_ref(&self, link_field: &str) -> Option<&str> {
        match link_field {
            "SIML" => Some(&self.siml),
            "SIOL" => Some(&self.siol),
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
            CycleCell::Val => CycleRead::ByName,
            _ => CycleRead::Absent,
        }
    }

    fn val(&self) -> Option<EpicsValue> {
        Some(EpicsValue::String(self.val.clone()))
    }

    fn software_event(&self) -> Option<&EventHandle> {
        self.epvt.as_ref()
    }

    /// INP (`devEventSoft.c:56`) and SIOL (`eventRecord.c:185`) are both
    /// `dbGetLink(.., DBR_STRING, .., 0, 0)`; the native read built the same
    /// text through the generic VAL put.
    fn input_link_request(&self, link_field: &str) -> crate::server::record::InputLinkRequest {
        use crate::server::record::{InputLinkRequest, LinkReadAs};
        InputLinkRequest::As(match link_field {
            "INP" | "SIOL" => LinkReadAs::String,
            _ => LinkReadAs::Native,
        })
    }

    /// `devEventSoft.c::read_event` (76-80): the `DBR_STRING` read replaces
    /// VAL only when it differs (`strcmp` then `strcpy`). The read is already
    /// VAL's own type, so it skips the generic put's field-shape lookups.
    fn soft_input_read(
        &mut self,
        value: Option<EpicsValue>,
        _dpvt: &mut crate::server::device_support::Dpvt,
    ) -> CaResult<()> {
        match value {
            Some(EpicsValue::String(mut s)) => {
                if s.as_bytes() != self.val.as_bytes() {
                    if s.len() >= MAX_STRING_SIZE {
                        s = PvString::from_bytes(&s.as_bytes()[..MAX_STRING_SIZE - 1]);
                    }
                    self.set_val_text(s);
                }
                Ok(())
            }
            Some(other) => self.set_val(other),
            None => Ok(()),
        }
    }

    fn get_field(&self, name: &str) -> Option<EpicsValue> {
        match name {
            "VAL" => Some(EpicsValue::String(self.val.clone())),
            "SIMM" => Some(EpicsValue::Short(self.simm)),
            "SIML" => Some(EpicsValue::String(self.siml.clone().into())),
            "SIOL" => Some(EpicsValue::String(self.siol.clone().into())),
            "SVAL" => Some(EpicsValue::String(self.sval.clone())),
            "SIMS" => Some(EpicsValue::Short(self.sims)),
            "SDLY" => Some(EpicsValue::Double(self.sdly)),
            _ => None,
        }
    }

    /// Same per-variant strictness the derive emits: a put of the wrong
    /// `EpicsValue` variant is a `TypeMismatch`, not a silent no-op.
    fn put_field(&mut self, name: &str, value: EpicsValue) -> CaResult<()> {
        self.validate_put(name, &value)?;
        match name {
            "VAL" => {
                let mut val = PvString::new();
                put_pv_string(&mut val, value, name)?;
                self.set_val_text(val);
            }
            "SIML" => put_string(&mut self.siml, value, name)?,
            "SIOL" => put_string(&mut self.siol, value, name)?,
            "SIMM" => put_short(&mut self.simm, value, name)?,
            "SIMS" => put_short(&mut self.sims, value, name)?,
            "SDLY" => put_double(&mut self.sdly, value, name)?,
            "SVAL" => put_pv_string(&mut self.sval, value, name)?,
            _ => return Err(CaError::FieldNotFound(name.to_string())),
        }
        self.on_put(name);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::record::RecordInstance;

    /// SIMM is `menu(menuYesNo)` served as DBR_ENUM. The base snapshot path
    /// promotes the stored Short to `Enum` and attaches the NO/YES labels.
    #[test]
    fn simm_snapshot_is_enum_with_yesno_labels() {
        let mut rec = EventRecord::default();
        rec.put_field("SIMM", EpicsValue::Short(1)).unwrap();
        assert_eq!(rec.menu_field_choices("SIMM"), Some(&["NO", "YES"][..]));
        let inst = RecordInstance::new("EV:SIMM".into(), rec);
        let snap = inst.snapshot_for_field("SIMM").unwrap();
        assert_eq!(snap.value, EpicsValue::Enum(1));
        assert_eq!(snap.enums.as_ref().unwrap().strings, vec!["NO", "YES"]);
    }

    /// Every VAL writer moves the handle with it: a load/put, the soft INP
    /// read, and the names `eventNameToHandle` maps to no event.
    #[test]
    fn every_val_write_settles_software_event() {
        let name = |r: &EventRecord| r.software_event().map(|e| e.name().to_owned());
        let mut rec = EventRecord::new(" 5.0 ");
        assert_eq!(name(&rec).as_deref(), Some("5"));
        rec.put_field("VAL", EpicsValue::String("shutter".into()))
            .unwrap();
        assert_eq!(name(&rec).as_deref(), Some("shutter"));
        rec.soft_input_read(
            Some(EpicsValue::String("0".into())),
            &mut Default::default(),
        )
        .unwrap();
        assert_eq!(name(&rec), None);
        rec.soft_input_read(
            Some(EpicsValue::String("7".into())),
            &mut Default::default(),
        )
        .unwrap();
        assert_eq!(name(&rec).as_deref(), Some("7"));
        rec.put_field("VAL", EpicsValue::String("".into())).unwrap();
        assert_eq!(name(&rec), None);
    }
}
