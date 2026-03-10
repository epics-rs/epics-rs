/// Output Mode Select
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Omsl {
    #[default]
    Supervisory = 0,
    ClosedLoop = 1,
}

impl From<i16> for Omsl {
    fn from(v: i16) -> Self {
        match v {
            1 => Self::ClosedLoop,
            _ => Self::Supervisory,
        }
    }
}

impl From<Omsl> for i16 {
    fn from(v: Omsl) -> Self {
        v as i16
    }
}

/// Invalid Output Action
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Ivoa {
    #[default]
    ContinueNormally = 0,
    DontDriveOutputs = 1,
    SetOutputToIvov = 2,
}

impl From<i16> for Ivoa {
    fn from(v: i16) -> Self {
        match v {
            1 => Self::DontDriveOutputs,
            2 => Self::SetOutputToIvov,
            _ => Self::ContinueNormally,
        }
    }
}

impl From<Ivoa> for i16 {
    fn from(v: Ivoa) -> Self {
        v as i16
    }
}

/// Alarm severity for ZSV/OSV/COSV fields
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AlarmSevr {
    #[default]
    None = 0,
    Minor = 1,
    Major = 2,
    Invalid = 3,
}

impl From<i16> for AlarmSevr {
    fn from(v: i16) -> Self {
        match v {
            1 => Self::Minor,
            2 => Self::Major,
            3 => Self::Invalid,
            _ => Self::None,
        }
    }
}

impl From<AlarmSevr> for i16 {
    fn from(v: AlarmSevr) -> Self {
        v as i16
    }
}

impl AlarmSevr {
    pub fn to_base(self) -> epics_base_rs::server::record::AlarmSeverity {
        match self {
            Self::None => epics_base_rs::server::record::AlarmSeverity::NoAlarm,
            Self::Minor => epics_base_rs::server::record::AlarmSeverity::Minor,
            Self::Major => epics_base_rs::server::record::AlarmSeverity::Major,
            Self::Invalid => epics_base_rs::server::record::AlarmSeverity::Invalid,
        }
    }
}
