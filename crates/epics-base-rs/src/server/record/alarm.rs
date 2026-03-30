/// Alarm severity levels matching EPICS base.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
#[repr(u16)]
pub enum AlarmSeverity {
    #[default]
    NoAlarm = 0,
    Minor = 1,
    Major = 2,
    Invalid = 3,
}

impl AlarmSeverity {
    pub fn from_u16(v: u16) -> Self {
        match v {
            0 => Self::NoAlarm,
            1 => Self::Minor,
            2 => Self::Major,
            3 => Self::Invalid,
            _ => Self::Invalid,
        }
    }
}

/// Analog alarm configuration — only for ai/ao/longin/longout.
#[derive(Clone, Debug)]
pub struct AnalogAlarmConfig {
    pub hihi: f64,
    pub high: f64,
    pub low: f64,
    pub lolo: f64,
    pub hhsv: AlarmSeverity,
    pub hsv: AlarmSeverity,
    pub lsv: AlarmSeverity,
    pub llsv: AlarmSeverity,
}

impl Default for AnalogAlarmConfig {
    fn default() -> Self {
        Self {
            hihi: 0.0,
            high: 0.0,
            low: 0.0,
            lolo: 0.0,
            hhsv: AlarmSeverity::NoAlarm,
            hsv: AlarmSeverity::NoAlarm,
            lsv: AlarmSeverity::NoAlarm,
            llsv: AlarmSeverity::NoAlarm,
        }
    }
}
