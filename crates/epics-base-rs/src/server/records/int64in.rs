use epics_macros_rs::EpicsRecord;

// int64in: 64-bit integer input.
// CA limitation: served as DBR_DOUBLE over Channel Access (f64, precision loss for |val|>2^53).
// Native i64 storage is lossless; precision is only lost at the CA wire boundary.
//
// Alarm threshold fields (HIHI/HIGH/LOW/LOLO/HHSV/HSV/LSV/LLSV) are intentionally absent
// from the field list so they route to RecordInstance::common.analog_alarm via
// put_common_field, matching the path used by longin/ao/ai.
#[derive(EpicsRecord)]
#[record(type = "int64in")]
pub struct Int64inRecord {
    #[field(type = "Int64")]
    pub val: i64,
    #[field(type = "String")]
    pub egu: String,
    #[field(type = "Double")]
    pub hopr: f64,
    #[field(type = "Double")]
    pub lopr: f64,
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
            val: 0,
            egu: String::new(),
            hopr: 0.0,
            lopr: 0.0,
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
            val,
            ..Default::default()
        }
    }
}
