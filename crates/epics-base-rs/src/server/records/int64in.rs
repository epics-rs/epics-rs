use epics_macros_rs::EpicsRecord;

// int64in: 64-bit integer input.
// CA protocol has no native 64-bit integer type; EPICS base serves int64 as DBR_DOUBLE over CA.
// All value/limit fields use Double to match that wire representation.
#[derive(EpicsRecord)]
#[record(type = "int64in")]
pub struct Int64inRecord {
    #[field(type = "Double")]
    pub val: f64,
    #[field(type = "String")]
    pub egu: String,
    #[field(type = "Double")]
    pub hopr: f64,
    #[field(type = "Double")]
    pub lopr: f64,
    #[field(type = "Double")]
    pub hihi: f64,
    #[field(type = "Double")]
    pub high: f64,
    #[field(type = "Double")]
    pub low: f64,
    #[field(type = "Double")]
    pub lolo: f64,
    #[field(type = "Short")]
    pub hhsv: i16,
    #[field(type = "Short")]
    pub hsv: i16,
    #[field(type = "Short")]
    pub lsv: i16,
    #[field(type = "Short")]
    pub llsv: i16,
    #[field(type = "Double")]
    pub hyst: f64,
    #[field(type = "Double")]
    pub lalm: f64,
    #[field(type = "Double")]
    pub adel: f64,
    #[field(type = "Double")]
    pub mdel: f64,
    #[field(type = "Double")]
    pub alst: f64,
    #[field(type = "Double")]
    pub mlst: f64,
    #[field(type = "Short")]
    pub simm: i16,
    #[field(type = "String")]
    pub siml: String,
    #[field(type = "String")]
    pub siol: String,
    #[field(type = "Short")]
    pub sims: i16,
}

impl Default for Int64inRecord {
    fn default() -> Self {
        Self {
            val: 0.0,
            egu: String::new(),
            hopr: 0.0,
            lopr: 0.0,
            hihi: 0.0,
            high: 0.0,
            low: 0.0,
            lolo: 0.0,
            hhsv: 0,
            hsv: 0,
            lsv: 0,
            llsv: 0,
            hyst: 0.0,
            lalm: 0.0,
            adel: 0.0,
            mdel: 0.0,
            alst: 0.0,
            mlst: 0.0,
            simm: 0,
            siml: String::new(),
            siol: String::new(),
            sims: 0,
        }
    }
}

impl Int64inRecord {
    pub fn new(val: i64) -> Self {
        Self {
            val: val as f64,
            ..Default::default()
        }
    }
}
