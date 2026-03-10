use crate::server::record::{AlarmSeverity, CommonFields};

/// Alarm status codes matching EPICS base.
pub mod alarm_status {
    pub const NO_ALARM: u16 = 0;
    pub const HIGH_ALARM: u16 = 1;
    pub const LOW_ALARM: u16 = 2;
    pub const HIHI_ALARM: u16 = 3;
    pub const LOLO_ALARM: u16 = 4;
    pub const STATE_ALARM: u16 = 5;
    pub const COS_ALARM: u16 = 6;
    pub const READ_ALARM: u16 = 7;
    pub const WRITE_ALARM: u16 = 8;
    pub const COMM_ALARM: u16 = 9;
    pub const TIMEOUT_ALARM: u16 = 10;
    pub const HW_LIMIT_ALARM: u16 = 11;
    pub const SCAN_ALARM: u16 = 12;
    pub const LINK_ALARM: u16 = 13;
    pub const DISABLE_ALARM: u16 = 14;
    pub const SIMM_ALARM: u16 = 15;
    pub const SOFT_ALARM: u16 = 16;
    pub const UDF_ALARM: u16 = 17;
    pub const CALC_ALARM: u16 = 18;
}

/// Event mask bits for monitor posting (matches EPICS DBE_*).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct EventMask(u16);

impl EventMask {
    pub const NONE: Self = Self(0);
    pub const VALUE: Self = Self(0x01);
    pub const LOG: Self = Self(0x02);
    pub const ALARM: Self = Self(0x04);
    pub const PROPERTY: Self = Self(0x08);

    pub fn is_empty(self) -> bool {
        self.0 == 0
    }

    pub fn contains(self, other: Self) -> bool {
        (self.0 & other.0) == other.0
    }

    pub fn intersects(self, other: Self) -> bool {
        (self.0 & other.0) != 0
    }

    pub fn bits(self) -> u16 {
        self.0
    }

    pub fn from_bits(bits: u16) -> Self {
        Self(bits)
    }
}

impl std::ops::BitOr for EventMask {
    type Output = Self;
    fn bitor(self, rhs: Self) -> Self {
        Self(self.0 | rhs.0)
    }
}

impl std::ops::BitOrAssign for EventMask {
    fn bitor_assign(&mut self, rhs: Self) {
        self.0 |= rhs.0;
    }
}

impl std::ops::BitAnd for EventMask {
    type Output = Self;
    fn bitand(self, rhs: Self) -> Self {
        Self(self.0 & rhs.0)
    }
}

/// Result of rec_gbl_reset_alarms: whether alarm state changed.
pub struct AlarmResetResult {
    pub alarm_changed: bool,
    pub prev_sevr: AlarmSeverity,
    pub prev_stat: u16,
}

/// Set new alarm severity if it's higher than current nsta/nsev.
/// Matches EPICS recGblSetSevr: only raises alarm, never lowers.
pub fn rec_gbl_set_sevr(common: &mut CommonFields, stat: u16, sevr: AlarmSeverity) {
    if (sevr as u16) > (common.nsev as u16) {
        common.nsta = stat;
        common.nsev = sevr;
    }
}

/// Transfer nsta/nsev to stat/sevr, detect alarm change, reset nsta/nsev.
/// Matches EPICS recGblResetAlarms. Call at end of process cycle.
pub fn rec_gbl_reset_alarms(common: &mut CommonFields) -> AlarmResetResult {
    let prev_sevr = common.sevr;
    let prev_stat = common.stat;

    // Transfer new alarm state
    common.sevr = common.nsev;
    common.stat = common.nsta;

    // Reset for next cycle
    common.nsev = AlarmSeverity::NoAlarm;
    common.nsta = alarm_status::NO_ALARM;

    let alarm_changed = common.sevr != prev_sevr || common.stat != prev_stat;

    AlarmResetResult {
        alarm_changed,
        prev_sevr,
        prev_stat,
    }
}

/// Check UDF alarm: if record is still undefined, raise UDF_ALARM.
pub fn rec_gbl_check_udf(common: &mut CommonFields) {
    if common.udf {
        rec_gbl_set_sevr(common, alarm_status::UDF_ALARM, AlarmSeverity::Invalid);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_set_sevr_raises() {
        let mut common = CommonFields::default();
        assert_eq!(common.nsev, AlarmSeverity::NoAlarm);

        rec_gbl_set_sevr(&mut common, alarm_status::HIGH_ALARM, AlarmSeverity::Minor);
        assert_eq!(common.nsev, AlarmSeverity::Minor);
        assert_eq!(common.nsta, alarm_status::HIGH_ALARM);
    }

    #[test]
    fn test_set_sevr_only_raises() {
        let mut common = CommonFields::default();
        rec_gbl_set_sevr(&mut common, alarm_status::HIHI_ALARM, AlarmSeverity::Major);
        rec_gbl_set_sevr(&mut common, alarm_status::HIGH_ALARM, AlarmSeverity::Minor);
        // Should keep the higher severity
        assert_eq!(common.nsev, AlarmSeverity::Major);
        assert_eq!(common.nsta, alarm_status::HIHI_ALARM);
    }

    #[test]
    fn test_reset_alarms_transfers() {
        let mut common = CommonFields::default();
        rec_gbl_set_sevr(&mut common, alarm_status::HIHI_ALARM, AlarmSeverity::Major);

        let result = rec_gbl_reset_alarms(&mut common);
        assert!(result.alarm_changed);
        assert_eq!(result.prev_sevr, AlarmSeverity::NoAlarm);
        assert_eq!(common.sevr, AlarmSeverity::Major);
        assert_eq!(common.stat, alarm_status::HIHI_ALARM);
        // nsta/nsev reset
        assert_eq!(common.nsev, AlarmSeverity::NoAlarm);
        assert_eq!(common.nsta, alarm_status::NO_ALARM);
    }

    #[test]
    fn test_reset_alarms_no_change() {
        let mut common = CommonFields::default();
        // No alarm set, reset should show no change
        let result = rec_gbl_reset_alarms(&mut common);
        assert!(!result.alarm_changed);
    }

    #[test]
    fn test_reset_alarms_clears() {
        let mut common = CommonFields::default();
        // First: set alarm
        common.sevr = AlarmSeverity::Major;
        common.stat = alarm_status::HIHI_ALARM;
        // Don't set nsta/nsev (no alarm this cycle)
        let result = rec_gbl_reset_alarms(&mut common);
        assert!(result.alarm_changed);
        assert_eq!(result.prev_sevr, AlarmSeverity::Major);
        assert_eq!(common.sevr, AlarmSeverity::NoAlarm);
    }

    #[test]
    fn test_check_udf() {
        let mut common = CommonFields::default();
        assert!(common.udf);
        rec_gbl_check_udf(&mut common);
        assert_eq!(common.nsev, AlarmSeverity::Invalid);
        assert_eq!(common.nsta, alarm_status::UDF_ALARM);
    }

    #[test]
    fn test_event_mask_ops() {
        let mask = EventMask::VALUE | EventMask::ALARM;
        assert!(mask.contains(EventMask::VALUE));
        assert!(mask.contains(EventMask::ALARM));
        assert!(!mask.contains(EventMask::LOG));
        assert!(mask.intersects(EventMask::VALUE));
        assert!(!mask.intersects(EventMask::PROPERTY));
        assert!(!mask.is_empty());
        assert!(EventMask::NONE.is_empty());
    }
}
