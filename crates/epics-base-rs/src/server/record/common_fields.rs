use std::time::SystemTime;

use super::alarm::{AlarmSeverity, AnalogAlarmConfig};
use super::scan::ScanType;

/// Common fields shared by all records.
#[derive(Clone, Debug)]
pub struct CommonFields {
    // Alarm state (current/result)
    pub sevr: AlarmSeverity,
    pub stat: u16,
    // New alarm state (pending, transferred by rec_gbl_reset_alarms)
    pub nsev: AlarmSeverity,
    pub nsta: u16,
    // Alarm acknowledgement
    pub acks: AlarmSeverity,
    pub ackt: bool,
    pub udf: bool,
    pub udfs: AlarmSeverity,
    // Scan
    pub scan: ScanType,
    pub sscn: ScanType,
    pub pini: bool,
    pub tpro: bool,
    pub bkpt: u8,
    // Links (raw strings)
    pub flnk: String,
    pub inp: String,
    pub out: String,
    // Device
    pub dtyp: String,
    // Timestamp
    pub time: SystemTime,
    pub tse: i16,
    pub tsel: String,
    // Analog alarm config (Some for analog record types)
    pub analog_alarm: Option<AnalogAlarmConfig>,
    // Access security group
    pub asg: String,
    // Description (moved from individual records)
    pub desc: String,
    // Phase/priority/event
    pub phas: i16,
    pub evnt: i16,
    pub prio: i16,
    // Disable support
    pub disv: i16,
    pub disa: i16,
    pub sdis: String,
    pub diss: AlarmSeverity,
    // Alarm hysteresis (analog records)
    pub hyst: f64,
    // Lock count (re-entrance counter)
    pub lcnt: i16,
    // Disable putfield from CA (default false)
    pub disp: bool,
    // Process control
    pub putf: bool,
    pub rpro: bool,
    // Fallback monitor/archive last-sent values for records without MLST/ALST fields
    pub mlst: Option<f64>,
    pub alst: Option<f64>,
}

impl Default for CommonFields {
    fn default() -> Self {
        Self {
            sevr: AlarmSeverity::NoAlarm,
            stat: 0,
            nsev: AlarmSeverity::NoAlarm,
            nsta: 0,
            acks: AlarmSeverity::NoAlarm,
            ackt: true,
            udf: true,
            udfs: AlarmSeverity::Invalid,
            scan: ScanType::Passive,
            sscn: ScanType::Passive,
            pini: false,
            tpro: false,
            bkpt: 0,
            flnk: String::new(),
            inp: String::new(),
            out: String::new(),
            dtyp: String::new(),
            time: SystemTime::UNIX_EPOCH,
            tse: 0,
            tsel: String::new(),
            analog_alarm: None,
            asg: "DEFAULT".to_string(),
            desc: String::new(),
            phas: 0,
            evnt: 0,
            prio: 0,
            disv: 1,
            disa: 0,
            sdis: String::new(),
            diss: AlarmSeverity::NoAlarm,
            hyst: 0.0,
            lcnt: 0,
            disp: false,
            putf: false,
            rpro: false,
            mlst: None,
            alst: None,
        }
    }
}
