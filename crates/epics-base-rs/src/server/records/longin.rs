use epics_macros_rs::EpicsRecord;

use crate::server::record::MENU_YES_NO;
use crate::types::PvString;

#[derive(EpicsRecord)]
#[record(type = "longin")]
pub struct LonginRecord {
    #[field(type = "Long")]
    pub val: i32,
    #[field(type = "PvStr")]
    pub egu: PvString,
    #[field(type = "Long")]
    pub hopr: i32,
    #[field(type = "Long")]
    pub lopr: i32,
    // Alarm thresholds
    #[field(type = "Long")]
    pub hihi: i32,
    #[field(type = "Long")]
    pub high: i32,
    #[field(type = "Long")]
    pub low: i32,
    #[field(type = "Long")]
    pub lolo: i32,
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
    // Deadband
    #[field(type = "Double")]
    pub adel: f64,
    #[field(type = "Double")]
    pub mdel: f64,
    // Alarm-range time-constant filter (longinRecord.c::checkAlarms:310-356).
    // AFTC > 0 low-pass-filters the integer alarmRange so transient
    // excursions don't immediately alarm; AFVL is the accumulator.
    #[field(type = "Double")]
    pub aftc: f64,
    #[field(type = "Double")]
    pub afvl: f64,
    #[field(type = "Double")]
    pub alst: f64,
    #[field(type = "Double")]
    pub mlst: f64,
    // SIMM is `DBF_MENU menu(menuYesNo)` (longinRecord.dbd.pod:475-479):
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

impl Default for LonginRecord {
    fn default() -> Self {
        Self {
            val: 0,
            egu: PvString::new(),
            hopr: 0,
            lopr: 0,
            hihi: 0,
            high: 0,
            low: 0,
            lolo: 0,
            hhsv: 0,
            hsv: 0,
            lsv: 0,
            llsv: 0,
            hyst: 0.0,
            lalm: 0.0,
            adel: 0.0,
            mdel: 0.0,
            aftc: 0.0,
            afvl: 0.0,
            alst: 0.0,
            mlst: 0.0,
            simm: 0,
            siml: String::new(),
            siol: String::new(),
            sims: 0,
        }
    }
}

impl LonginRecord {
    pub fn new(val: i32) -> Self {
        Self {
            val,
            ..Default::default()
        }
    }
}
