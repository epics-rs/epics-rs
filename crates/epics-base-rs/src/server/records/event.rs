use epics_macros_rs::EpicsRecord;

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
    #[field(type = "Short")]
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
