use epics_macros::EpicsRecord;

#[derive(EpicsRecord)]
#[record(type = "longout")]
pub struct LongoutRecord {
    #[field(type = "Long")]
    pub val: i32,
    #[field(type = "String")]
    pub egu: String,
    #[field(type = "Long")]
    pub hopr: i32,
    #[field(type = "Long")]
    pub lopr: i32,
    #[field(type = "Long")]
    pub drvh: i32,
    #[field(type = "Long")]
    pub drvl: i32,
    #[field(type = "Double")]
    pub adel: f64,
    #[field(type = "Double")]
    pub mdel: f64,
    #[field(type = "Double")]
    pub alst: f64,
    #[field(type = "Double")]
    pub mlst: f64,
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

impl Default for LongoutRecord {
    fn default() -> Self {
        Self {
            val: 0,
            egu: String::new(),
            hopr: 0,
            lopr: 0,
            drvh: i32::MAX,
            drvl: i32::MIN,
            adel: 0.0,
            mdel: 0.0,
            alst: 0.0,
            mlst: 0.0,
            omsl: 0,
            dol: String::new(),
            simm: 0,
            siml: String::new(),
            siol: String::new(),
            sims: 0,
        }
    }
}

impl LongoutRecord {
    pub fn new(val: i32) -> Self {
        Self {
            val,
            ..Default::default()
        }
    }
}
