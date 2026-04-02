use std::time::SystemTime;

use crate::types::EpicsValue;

/// Alarm status and severity.
#[derive(Debug, Clone, Default)]
pub struct AlarmInfo {
    pub status: u16,
    pub severity: u16,
}

/// Display/graphic metadata for numeric types.
#[derive(Debug, Clone, Default)]
pub struct DisplayInfo {
    pub units: String,
    pub precision: i16,
    pub upper_disp_limit: f64,
    pub lower_disp_limit: f64,
    pub upper_alarm_limit: f64,
    pub upper_warning_limit: f64,
    pub lower_warning_limit: f64,
    pub lower_alarm_limit: f64,
}

/// Control limits (DRVH/DRVL for output records, or HOPR/LOPR).
#[derive(Debug, Clone, Default)]
pub struct ControlInfo {
    pub upper_ctrl_limit: f64,
    pub lower_ctrl_limit: f64,
}

/// Enum state strings (up to 16 states, each max 26 chars on wire).
#[derive(Debug, Clone, Default)]
pub struct EnumInfo {
    pub strings: Vec<String>,
}

/// Unified internal state representation for a PV read.
#[derive(Debug, Clone)]
pub struct Snapshot {
    pub value: EpicsValue,
    pub alarm: AlarmInfo,
    pub timestamp: SystemTime,
    pub display: Option<DisplayInfo>,
    pub control: Option<ControlInfo>,
    pub enums: Option<EnumInfo>,
}

impl Snapshot {
    /// Create a new snapshot with minimal metadata (no display/control/enum info).
    pub fn new(value: EpicsValue, status: u16, severity: u16, timestamp: SystemTime) -> Self {
        Self {
            value,
            alarm: AlarmInfo { status, severity },
            timestamp,
            display: None,
            control: None,
            enums: None,
        }
    }
}

/// Classification of DBR type ranges.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DbrClass {
    Plain,
    Sts,
    Time,
    Gr,
    Ctrl,
}

impl DbrClass {
    /// Classify a DBR type code into its range.
    pub fn from_dbr_type(dbr_type: u16) -> Option<Self> {
        match dbr_type {
            0..=6 => Some(DbrClass::Plain),
            7..=13 => Some(DbrClass::Sts),
            14..=20 => Some(DbrClass::Time),
            21..=27 => Some(DbrClass::Gr),
            28..=34 => Some(DbrClass::Ctrl),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_snapshot_construction() {
        let snap = Snapshot::new(EpicsValue::Double(42.0), 0, 0, SystemTime::UNIX_EPOCH);
        assert_eq!(snap.alarm.status, 0);
        assert_eq!(snap.alarm.severity, 0);
        assert!(snap.display.is_none());
        assert!(snap.control.is_none());
        assert!(snap.enums.is_none());
    }

    #[test]
    fn test_snapshot_with_metadata() {
        let mut snap = Snapshot::new(EpicsValue::Double(3.14), 1, 2, SystemTime::UNIX_EPOCH);
        snap.display = Some(DisplayInfo {
            units: "degC".to_string(),
            precision: 3,
            upper_disp_limit: 100.0,
            lower_disp_limit: -50.0,
            upper_alarm_limit: 90.0,
            upper_warning_limit: 80.0,
            lower_warning_limit: -20.0,
            lower_alarm_limit: -40.0,
        });
        snap.control = Some(ControlInfo {
            upper_ctrl_limit: 100.0,
            lower_ctrl_limit: -50.0,
        });
        let disp = snap.display.as_ref().unwrap();
        assert_eq!(disp.units, "degC");
        assert_eq!(disp.precision, 3);
        assert_eq!(snap.control.as_ref().unwrap().upper_ctrl_limit, 100.0);
    }

    #[test]
    fn test_dbr_class_plain() {
        for t in 0..=6 {
            assert_eq!(DbrClass::from_dbr_type(t), Some(DbrClass::Plain));
        }
    }

    #[test]
    fn test_dbr_class_all_ranges() {
        // STS: 7-13
        for t in 7..=13 {
            assert_eq!(DbrClass::from_dbr_type(t), Some(DbrClass::Sts));
        }
        // TIME: 14-20
        for t in 14..=20 {
            assert_eq!(DbrClass::from_dbr_type(t), Some(DbrClass::Time));
        }
        // GR: 21-27
        for t in 21..=27 {
            assert_eq!(DbrClass::from_dbr_type(t), Some(DbrClass::Gr));
        }
        // CTRL: 28-34
        for t in 28..=34 {
            assert_eq!(DbrClass::from_dbr_type(t), Some(DbrClass::Ctrl));
        }
    }

    #[test]
    fn test_dbr_class_invalid() {
        assert_eq!(DbrClass::from_dbr_type(35), None);
        assert_eq!(DbrClass::from_dbr_type(100), None);
        assert_eq!(DbrClass::from_dbr_type(u16::MAX), None);
    }
}
