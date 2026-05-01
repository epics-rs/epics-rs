use epics_macros_rs::EpicsRecord;

// Event record: triggers monitor posts and FLNK on process.
// VAL is a short integer (typically unused); the record exists purely as a scan trigger.
#[derive(EpicsRecord)]
#[record(type = "event")]
pub struct EventRecord {
    #[field(type = "Short")]
    pub val: i16,
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
            val: 0,
            simm: 0,
            siml: String::new(),
            siol: String::new(),
            sims: 0,
        }
    }
}

impl EventRecord {
    pub fn new(val: i16) -> Self {
        Self {
            val,
            ..Default::default()
        }
    }
}
