use epics_macros_rs::EpicsRecord;

use crate::server::record::MENU_YES_NO;

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
/// routes the event through [`PvDatabase::post_event_named`] — the
/// record stays a pure state machine with no direct DB access.
#[derive(EpicsRecord)]
#[record(type = "event")]
pub struct EventRecord {
    /// Event name to post. DBF_STRING in C (was a numeric subscript
    /// pre-EPICS-7; a numeric string still works for back-compat).
    #[field(type = "String")]
    pub val: String,
    // SIMM is `DBF_MENU menu(menuYesNo)` (eventRecord.dbd.pod:152-156):
    // the two-choice NO/YES simulation menu, served as DBR_ENUM.
    #[field(type = "Short", menu_choices = MENU_YES_NO)]
    pub simm: i16,
    #[field(type = "String")]
    pub siml: String,
    #[field(type = "String")]
    pub siol: String,
    #[field(type = "Short")]
    pub sims: i16,
}

impl Default for EventRecord {
    fn default() -> Self {
        Self {
            val: String::new(),
            simm: 0,
            siml: String::new(),
            siol: String::new(),
            sims: 0,
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::record::{Record, RecordInstance};
    use crate::types::EpicsValue;

    /// SIMM is `menu(menuYesNo)` served as DBR_ENUM. The derive macro must
    /// wire the `menu_choices = MENU_YES_NO` attribute into
    /// `menu_field_choices`, and the base snapshot path then promotes the
    /// stored Short to `Enum` and attaches the NO/YES labels.
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
