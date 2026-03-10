use epics_macros::EpicsRecord;

#[derive(EpicsRecord)]
#[record(type = "bo")]
pub struct BoRecord {
    #[field(type = "Enum")]
    pub val: u16,
    #[field(type = "String")]
    pub znam: String,
    #[field(type = "String")]
    pub onam: String,
    #[field(type = "Short")]
    pub zsv: i16,
    #[field(type = "Short")]
    pub osv: i16,
    #[field(type = "Short")]
    pub cosv: i16,
    #[field(type = "Short")]
    pub omsl: i16,
    #[field(type = "String")]
    pub dol: String,
    #[field(type = "Short")]
    pub simm: i16,
    #[field(type = "String")]
    pub siml: String,
    #[field(type = "String")]
    pub siol: String,
    #[field(type = "Short")]
    pub sims: i16,
}

impl Default for BoRecord {
    fn default() -> Self {
        Self {
            val: 0,
            znam: String::new(),
            onam: String::new(),
            zsv: 0,
            osv: 0,
            cosv: 0,
            omsl: 0,
            dol: String::new(),
            simm: 0,
            siml: String::new(),
            siol: String::new(),
            sims: 0,
        }
    }
}

impl BoRecord {
    pub fn new(val: u16) -> Self {
        Self {
            val,
            ..Default::default()
        }
    }
}
