use crate::types::{EpicsValue, PvString, WallTime};

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
    /// Engineering units (record EGU). Byte-preserving: CA `DBR_STRING`
    /// and PVA `display.units` carry raw, not-guaranteed-UTF-8 bytes, so a
    /// non-UTF-8 EGU must reach the wire unmangled (pvxs stores the wire
    /// string verbatim, `pvaproto.h:403`). A `String` here forced a lossy
    /// UTF-8 round-trip at this metadata boundary.
    pub units: PvString,
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
///
/// Byte-preserving like [`DisplayInfo::units`]: enum choice labels are
/// raw wire/record bytes with no UTF-8 guarantee, so they must reach the
/// CA `DBR_GR_ENUM` slots and the PVA `value.choices` array unmangled.
#[derive(Debug, Clone, Default)]
pub struct EnumInfo {
    pub strings: Vec<PvString>,
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
    /// Wall-clock timestamp with full EPICS `nsec` precision on every platform.
    /// Held as [`WallTime`] rather than [`SystemTime`] because the latter is
    /// 100 ns-granular on Windows and truncated externally supplied `nsec`
    /// (wire decode, PVA PUT, `Q:time:tag` split). [`Snapshot::new`] still
    /// accepts a `SystemTime` (via `Into`), so "now"-style call sites are
    /// unchanged; precise integer sources construct it with
    /// [`WallTime::from_unix`].
    pub timestamp: WallTime,
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

/// Split `snap.timestamp.nanoseconds` across `nanoseconds` / `user_tag`
/// along `nsec_mask` — the mask a record's `info(Q:time:tag,
/// "nsec:lsb:N")` resolves to (pvxs `MappingInfo::updateNsecMask`,
/// `typeutils.cpp:79-88`).
///
/// Mirrors pvxs `iocsource.cpp:239-248` byte for byte:
///
/// ```text
/// node["timeStamp.nanoseconds"] = meta.time.nsec & ~info.nsecMask;
/// if(info.nsecMask)
///     utag = meta.time.nsec & info.nsecMask;
/// ```
///
/// The mask — not a bit count — is the parameter, so "the feature is
/// off" has exactly one representation: `nsec_mask == 0`. pvxs gates the
/// `userTag` override on the same test, and `nsec & ~0` is the identity,
/// so a zero mask leaves both fields untouched. With the typical
/// `nsec:lsb:20` mask (`0x000F_FFFF`), `nanoseconds` keeps wall-clock
/// precision down to ~1 µs while the userTag carries a 20-bit event id.
pub fn apply_nsec_mask(snap: &mut Snapshot, nsec_mask: u64) {
    if nsec_mask == 0 {
        return;
    }
    // pvxs holds `nsecMask` as `uint64_t` and applies it to the
    // `epicsUInt32` `nsec`, so only the low 32 bits can ever take effect.
    let mask = nsec_mask as u32;
    let secs = snap.timestamp.unix_secs();
    let nanos = snap.timestamp.subsec_nanos();
    snap.user_tag = (nanos & mask) as i32;
    snap.timestamp = WallTime::from_unix(secs, nanos & !mask);
}

impl Snapshot {
    /// Create a new snapshot with minimal metadata (no display/control/enum info).
    ///
    /// `timestamp` accepts anything convertible into [`WallTime`] — a plain
    /// [`SystemTime`] (for "now"-style clock reads, which lose no precision the
    /// OS clock already lacks) or a [`WallTime::from_unix`] built from an exact
    /// integer `(secs, nsec)` taken off the wire.
    pub fn new(
        value: EpicsValue,
        status: u16,
        severity: u16,
        timestamp: impl Into<WallTime>,
    ) -> Self {
        Self {
            value,
            alarm: AlarmInfo {
                status,
                severity,
                ackt: None,
                acks: None,
            },
            timestamp: timestamp.into(),
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
    use std::time::SystemTime;

    /// With the `nsec:lsb:20` mask, the low 20 nanosecond bits land in
    /// userTag and are cleared from the timestamp. Use a known nanosecond
    /// value so the bit math is easy to verify.
    #[test]
    fn nsec_mask_extracts_user_tag() {
        let nanos: u32 = 123_456_789; // 0x075BCD15 — sub-second
        // Inject the exact integer (secs, nsec); a `SystemTime` would truncate
        // these low nanosecond digits to 100 ns on Windows before the split.
        let ts = WallTime::from_unix(42, nanos);
        let mut snap = Snapshot::new(EpicsValue::Double(0.0), 0, 0, ts);
        apply_nsec_mask(&mut snap, (1 << 20) - 1);
        let mask: u32 = (1 << 20) - 1;
        let expected_user_tag = (nanos & mask) as i32;
        let expected_nanos = nanos & !mask;
        assert_eq!(snap.user_tag, expected_user_tag);
        assert_eq!(snap.timestamp.unix_secs(), 42);
        assert_eq!(snap.timestamp.subsec_nanos(), expected_nanos);
    }

    /// The `nsec:lsb:1` mask splits the single LSB into the userTag.
    #[test]
    fn nsec_mask_n1_keeps_high_bits() {
        let ts = WallTime::from_unix(0, 7); // ...0111
        let mut snap = Snapshot::new(EpicsValue::Double(0.0), 0, 0, ts);
        apply_nsec_mask(&mut snap, 1);
        assert_eq!(snap.user_tag, 1);
        assert_eq!(snap.timestamp.subsec_nanos(), 6);
    }

    /// Mask 0 (pvxs's `nsecMask` initialiser — no `Q:time:tag`, or one that
    /// does not parse) is the off state: `nsec & ~0` is the identity and
    /// pvxs's `if(info.nsecMask)` gate leaves `userTag` at the record's own
    /// UTAG. Neither field may be touched.
    #[test]
    fn nsec_mask_zero_is_a_no_op() {
        let ts = WallTime::from_unix(9, 123_456_789);
        let mut snap = Snapshot::new(EpicsValue::Double(0.0), 0, 0, ts);
        snap.user_tag = -7; // record's own UTAG
        apply_nsec_mask(&mut snap, 0);
        assert_eq!(snap.user_tag, -7);
        assert_eq!(snap.timestamp.unix_secs(), 9);
        assert_eq!(snap.timestamp.subsec_nanos(), 123_456_789);
    }

    /// `nsec:lsb:31` — the mask pvxs builds one past the old Rust `1..=30`
    /// clamp. `nanoseconds` is always < 1e9 < 2^30, so a 31-bit mask moves
    /// the whole nanosecond field into `userTag` and publishes 0 nanoseconds
    /// (pvxs `iocsource.cpp:239-248`).
    #[test]
    fn nsec_mask_31_moves_all_nanoseconds_to_user_tag() {
        let nanos: u32 = 999_999_999;
        let ts = WallTime::from_unix(1, nanos);
        let mut snap = Snapshot::new(EpicsValue::Double(0.0), 0, 0, ts);
        apply_nsec_mask(&mut snap, (1u64 << 31) - 1);
        assert_eq!(snap.user_tag, nanos as i32);
        assert_eq!(snap.timestamp.subsec_nanos(), 0);
        assert_eq!(snap.timestamp.unix_secs(), 1);
    }

    /// pvxs holds `nsecMask` as a `uint64_t` but applies it to the 32-bit
    /// `nsec`, so a mask wider than 32 bits behaves exactly like the
    /// all-ones 32-bit mask.
    #[test]
    fn nsec_mask_wider_than_32_bits_clears_all_nanoseconds() {
        let nanos: u32 = 123_456_789;
        let ts = WallTime::from_unix(3, nanos);
        let mut snap = Snapshot::new(EpicsValue::Double(0.0), 0, 0, ts);
        apply_nsec_mask(&mut snap, (1u64 << 40) - 1);
        assert_eq!(snap.user_tag, nanos as i32);
        assert_eq!(snap.timestamp.subsec_nanos(), 0);
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
            units: "degC".into(),
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
