use epics_macros_rs::EpicsRecord;

#[derive(EpicsRecord)]
#[record(type = "fanout")]
pub struct FanoutRecord {
    #[field(type = "Enum")]
    pub val: u16,
    #[field(type = "Short")]
    pub selm: i16,
    #[field(type = "Short")]
    pub seln: i16,
    #[field(type = "String")]
    pub lnk1: String,
    #[field(type = "String")]
    pub lnk2: String,
    #[field(type = "String")]
    pub lnk3: String,
    #[field(type = "String")]
    pub lnk4: String,
    #[field(type = "String")]
    pub lnk5: String,
    #[field(type = "String")]
    pub lnk6: String,
}

impl Default for FanoutRecord {
    fn default() -> Self {
        Self {
            val: 0,
            selm: 0,
            seln: 0,
            lnk1: String::new(),
            lnk2: String::new(),
            lnk3: String::new(),
            lnk4: String::new(),
            lnk5: String::new(),
            lnk6: String::new(),
        }
    }
}

impl FanoutRecord {
    pub fn new() -> Self {
        Self::default()
    }

    /// Get all non-empty link targets.
    pub fn links(&self) -> Vec<&str> {
        [
            &self.lnk1, &self.lnk2, &self.lnk3, &self.lnk4, &self.lnk5, &self.lnk6,
        ]
        .iter()
        .filter(|s| !s.is_empty())
        .map(|s| s.as_str())
        .collect()
    }
}
