use crate::error::{CaError, CaResult};

/// Scan types matching EPICS base SCAN field menu.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash, Default)]
#[repr(u16)]
pub enum ScanType {
    #[default]
    Passive = 0,
    Event = 1,
    IoIntr = 2,
    Sec10 = 3,
    Sec5 = 4,
    Sec2 = 5,
    Sec1 = 6,
    Sec05 = 7,
    Sec02 = 8,
    Sec01 = 9,
}

impl ScanType {
    pub fn from_u16(v: u16) -> Self {
        match v {
            0 => Self::Passive,
            1 => Self::Event,
            2 => Self::IoIntr,
            3 => Self::Sec10,
            4 => Self::Sec5,
            5 => Self::Sec2,
            6 => Self::Sec1,
            7 => Self::Sec05,
            8 => Self::Sec02,
            9 => Self::Sec01,
            _ => Self::Passive,
        }
    }

    pub fn from_str(s: &str) -> CaResult<Self> {
        let s = s.trim();
        let lower = s.to_ascii_lowercase();
        match lower.as_str() {
            "passive" => Ok(Self::Passive),
            "event" => Ok(Self::Event),
            "i/o intr" | "iointr" => Ok(Self::IoIntr),
            "10 second" => Ok(Self::Sec10),
            "5 second" => Ok(Self::Sec5),
            "2 second" => Ok(Self::Sec2),
            "1 second" => Ok(Self::Sec1),
            ".5 second" | "0.5 second" => Ok(Self::Sec05),
            ".2 second" | "0.2 second" => Ok(Self::Sec02),
            ".1 second" | "0.1 second" => Ok(Self::Sec01),
            other => {
                if let Ok(v) = other.parse::<u16>() {
                    Ok(Self::from_u16(v))
                } else {
                    Err(CaError::InvalidValue(format!("unknown scan type: '{s}'")))
                }
            }
        }
    }

    /// Return the interval duration for periodic scan types.
    pub fn interval(&self) -> Option<std::time::Duration> {
        match self {
            Self::Sec10 => Some(std::time::Duration::from_secs(10)),
            Self::Sec5 => Some(std::time::Duration::from_secs(5)),
            Self::Sec2 => Some(std::time::Duration::from_secs(2)),
            Self::Sec1 => Some(std::time::Duration::from_secs(1)),
            Self::Sec05 => Some(std::time::Duration::from_millis(500)),
            Self::Sec02 => Some(std::time::Duration::from_millis(200)),
            Self::Sec01 => Some(std::time::Duration::from_millis(100)),
            _ => None,
        }
    }
}

impl std::fmt::Display for ScanType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Passive => write!(f, "Passive"),
            Self::Event => write!(f, "Event"),
            Self::IoIntr => write!(f, "I/O Intr"),
            Self::Sec10 => write!(f, "10 second"),
            Self::Sec5 => write!(f, "5 second"),
            Self::Sec2 => write!(f, "2 second"),
            Self::Sec1 => write!(f, "1 second"),
            Self::Sec05 => write!(f, ".5 second"),
            Self::Sec02 => write!(f, ".2 second"),
            Self::Sec01 => write!(f, ".1 second"),
        }
    }
}
