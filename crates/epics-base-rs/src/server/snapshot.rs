use std::time::SystemTime;

use crate::types::EpicsValue;

/// Alarm status and severity.
#[derive(Debug, Clone, Default)]
pub struct AlarmInfo {
    pub status: u16,
    pub severity: u16,
    /// Acknowledge transient (record ACKT field). Populated when
    /// callers want DBR_STSACK_STRING responses to carry it; otherwise
    /// `None` and the encoder substitutes 0.
    pub ackt: Option<u16>,
    /// Acknowledge severity (record ACKS field).
    pub acks: Option<u16>,
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
    /// Display format hint (0=Default, 1=String, 2=Binary, 3=Decimal,
    /// 4=Hex, 5=Exponential, 6=Engineering). From record's Q:form info tag.
    pub form: i16,
    /// Record description (DESC field).
    pub description: String,
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
///
/// `#[non_exhaustive]` so future field additions (e.g. another DBR
/// variant's metadata, a new pvxs-style annotation) don't break
/// external code that constructs `Snapshot` via struct literal.
/// Internal call sites use `Snapshot::new` + field assignment; that
/// pattern is forward-compatible.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct Snapshot {
    pub value: EpicsValue,
    pub alarm: AlarmInfo,
    pub timestamp: SystemTime,
    pub display: Option<DisplayInfo>,
    pub control: Option<ControlInfo>,
    pub enums: Option<EnumInfo>,
    /// Timestamp user tag (from Q:time:tag info, nsec LSB splitting).
    pub user_tag: i32,
    /// IOC record-type class name. Populated by the server before
    /// encoding a `DBR_CLASS_NAME` (38) response so the client receives
    /// the actual recordType (`ai`, `bo`, `waveform`, …) rather than an
    /// empty string. `None` for non-record-backed channels.
    pub class_name: Option<String>,
}

/// BR-R35: extract the low `n` bits of `snap.timestamp.nanoseconds`
/// into `snap.user_tag` and zero those bits in the timestamp.
///
/// Mirrors pvxs `iocsource.cpp:240` — for a record with
/// `info(Q:time:tag, "nsec:lsb:N")`, the IOC publishes the timestamp
/// with the low N nanosecond bits stripped into `timeStamp.userTag`,
/// which clients use as a pulse-id / event-id. With N=20 (typical),
/// `nanoseconds` keeps wall-clock precision down to ~1µs while the
/// userTag carries a 20-bit event id.
///
/// `n` must be in `1..=30`; callers parse the info value and clamp
/// before reaching this helper (out-of-range is a no-op at the
/// caller site).
pub fn apply_nsec_lsb_split(snap: &mut Snapshot, n: u8) {
    use std::time::{Duration, UNIX_EPOCH};

    debug_assert!((1..=30).contains(&n));
    let dur = match snap.timestamp.duration_since(UNIX_EPOCH) {
        Ok(d) => d,
        Err(_) => return,
    };
    let secs = dur.as_secs();
    let nanos = dur.subsec_nanos();
    let mask = (1u32 << n) - 1;
    let user_tag_bits = nanos & mask;
    let cleared_nanos = nanos & !mask;
    snap.user_tag = user_tag_bits as i32;
    snap.timestamp = UNIX_EPOCH + Duration::new(secs, cleared_nanos);
}

impl Snapshot {
    /// Create a new snapshot with minimal metadata (no display/control/enum info).
    pub fn new(value: EpicsValue, status: u16, severity: u16, timestamp: SystemTime) -> Self {
        Self {
            value,
            alarm: AlarmInfo {
                status,
                severity,
                ackt: None,
                acks: None,
            },
            timestamp,
            display: None,
            control: None,
            enums: None,
            user_tag: 0,
            class_name: None,
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

    /// BR-R35: with N=20, the low 20 nanosecond bits land in userTag
    /// and are cleared from the timestamp. Use a known nanosecond
    /// value so the bit math is easy to verify.
    #[test]
    fn nsec_lsb_split_extracts_user_tag() {
        use std::time::{Duration, UNIX_EPOCH};
        let nanos: u32 = 123_456_789; // 0x075BCD15 — sub-second
        let ts = UNIX_EPOCH + Duration::new(42, nanos);
        let mut snap = Snapshot::new(EpicsValue::Double(0.0), 0, 0, ts);
        apply_nsec_lsb_split(&mut snap, 20);
        let mask: u32 = (1 << 20) - 1;
        let expected_user_tag = (nanos & mask) as i32;
        let expected_nanos = nanos & !mask;
        assert_eq!(snap.user_tag, expected_user_tag);
        let dur = snap.timestamp.duration_since(UNIX_EPOCH).unwrap();
        assert_eq!(dur.as_secs(), 42);
        assert_eq!(dur.subsec_nanos(), expected_nanos);
    }

    /// BR-R35: N=1 splits the single LSB into the userTag.
    #[test]
    fn nsec_lsb_split_n1_keeps_high_bits() {
        use std::time::{Duration, UNIX_EPOCH};
        let ts = UNIX_EPOCH + Duration::new(0, 7); // ...0111
        let mut snap = Snapshot::new(EpicsValue::Double(0.0), 0, 0, ts);
        apply_nsec_lsb_split(&mut snap, 1);
        assert_eq!(snap.user_tag, 1);
        let dur = snap.timestamp.duration_since(UNIX_EPOCH).unwrap();
        assert_eq!(dur.subsec_nanos(), 6);
    }

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
            ..Default::default()
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
