use epics_macros::EpicsRecord;

#[derive(EpicsRecord)]
#[record(type = "stringin")]
pub struct StringinRecord {
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

impl Default for StringinRecord {
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

impl StringinRecord {
    pub fn new(val: &str) -> Self {
        Self {
            val: val.to_string(),
            ..Default::default()
        }
    }
}
