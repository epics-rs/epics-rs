use crate::error::{CaError, CaResult};
use crate::server::record::{MENU_YES_NO, Record};
use crate::types::{EpicsValue, PvString};

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
    /// pre-EPICS-7; a numeric string still works for back-compat).
    pub val: String,
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
            val: String::new(),
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
        Self {
            val: val.into(),
            ..Default::default()
        }
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
    fn record_type(&self) -> &'static str {
        "event"
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

    /// `SIMM` is `menu(menuYesNo)`, served as DBR_ENUM with the NO/YES labels.
    fn menu_field_choices(&self, field: &str) -> Option<&'static [&'static str]> {
        match field {
            "SIMM" => Some(MENU_YES_NO),
            _ => None,
        }
    }

    fn get_field(&self, name: &str) -> Option<EpicsValue> {
        match name {
            "VAL" => Some(EpicsValue::String(self.val.clone().into())),
            "SIMM" => Some(EpicsValue::Short(self.simm)),
            "SIML" => Some(EpicsValue::String(self.siml.clone().into())),
            "SIOL" => Some(EpicsValue::String(self.siol.clone().into())),
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
            "VAL" => put_string(&mut self.val, value, name)?,
            "SIML" => put_string(&mut self.siml, value, name)?,
            "SIOL" => put_string(&mut self.siol, value, name)?,
            "SIMM" => put_short(&mut self.simm, value, name)?,
            "SIMS" => put_short(&mut self.sims, value, name)?,
            "SDLY" => put_double(&mut self.sdly, value, name)?,
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
}
