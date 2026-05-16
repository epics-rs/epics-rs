use crate::server::record::{AlarmSeverity, CommonFields};

/// Alarm status codes matching EPICS base's `menuAlarmStat.dbd` /
/// `epicsAlarmCondition` (libcom/src/misc/alarm.h) wire format. The
/// numeric values are baked into the CA wire protocol's `stat` byte,
/// so they MUST match C — a Rust IOC sending `LINK_ALARM = 13` would
/// be decoded by a C client as `SCAN` (which is 13 in C).
pub mod alarm_status {
    pub const NO_ALARM: u16 = 0;
    pub const READ_ALARM: u16 = 1;
    pub const WRITE_ALARM: u16 = 2;
    pub const HIHI_ALARM: u16 = 3;
    pub const HIGH_ALARM: u16 = 4;
    pub const LOLO_ALARM: u16 = 5;
    pub const LOW_ALARM: u16 = 6;
    pub const STATE_ALARM: u16 = 7;
    pub const COS_ALARM: u16 = 8;
    pub const COMM_ALARM: u16 = 9;
    pub const TIMEOUT_ALARM: u16 = 10;
    pub const HW_LIMIT_ALARM: u16 = 11;
    pub const CALC_ALARM: u16 = 12;
    pub const SCAN_ALARM: u16 = 13;
    pub const LINK_ALARM: u16 = 14;
    pub const SOFT_ALARM: u16 = 15;
    pub const BAD_SUB_ALARM: u16 = 16;
    pub const UDF_ALARM: u16 = 17;
    pub const DISABLE_ALARM: u16 = 18;
    pub const SIMM_ALARM: u16 = 19;
    pub const READ_ACCESS_ALARM: u16 = 20;
    pub const WRITE_ACCESS_ALARM: u16 = 21;
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
    /// True iff `amsg` value changed in this reset cycle (epics-base PR #568).
    pub amsg_changed: bool,
    /// True iff `acks` value was raised in this reset cycle (C parity
    /// with `recGblResetAlarms` — `acks` tracks the highest unacknowledged
    /// severity so operators can clear sticky alarms via a CA put).
    pub acks_changed: bool,
}

/// Set new alarm severity if it's higher than current nsta/nsev.
/// Matches EPICS `recGblSetSevr` (recGbl.c:258-261): `recGblSetSevr`
/// delegates to `recGblSetSevrMsg(..., NULL)` → `recGblSetSevrVMsg`
/// with `msg == NULL`. When the new severity raises the pending
/// state, the `msg == NULL` branch (recGbl.c:248-250) executes
/// `prec->namsg[0] = '\0'` — i.e. it **clears** any pending alarm
/// message. A stale `namsg` written by an earlier MSS link (or
/// `evaluate_alarms`) must not survive when a later, higher-severity
/// no-message alarm raises the record severity; otherwise the
/// record's final `amsg` carries an unrelated upstream record's
/// alarm text.
pub fn rec_gbl_set_sevr(common: &mut CommonFields, stat: u16, sevr: AlarmSeverity) {
    if (sevr as u16) > (common.nsev as u16) {
        common.nsta = stat;
        common.nsev = sevr;
        // C `msg == NULL` branch clears the pending message.
        common.namsg.clear();
    }
}

/// Set new alarm severity AND attach an alarm message (epics-base PR
/// #568 `recGblSetSevrMsg`). Same "raise only" rule as `rec_gbl_set_sevr`;
/// when the new severity raises the pending state, both `nsta`/`nsev`
/// AND `namsg` are written together. Empty `msg` clears the pending
/// message — non-empty replaces it.
pub fn rec_gbl_set_sevr_msg(
    common: &mut CommonFields,
    stat: u16,
    sevr: AlarmSeverity,
    msg: impl Into<String>,
) {
    if (sevr as u16) > (common.nsev as u16) {
        common.nsta = stat;
        common.nsev = sevr;
        common.namsg = msg.into();
    }
}

/// Transfer nsta/nsev to stat/sevr, detect alarm change, reset nsta/nsev.
/// Matches EPICS recGblResetAlarms. Call at end of process cycle.
///
/// Mirrors epics-base PR #566 — the alarm-message string (`amsg`) is
/// transferred from `namsg` alongside the severity / status, and
/// `namsg` is cleared for the next cycle. Records that did not call
/// `rec_gbl_set_sevr_msg` this cycle end up with an empty `amsg`.
pub fn rec_gbl_reset_alarms(common: &mut CommonFields) -> AlarmResetResult {
    let prev_sevr = common.sevr;
    let prev_stat = common.stat;
    let prev_amsg = std::mem::take(&mut common.amsg);
    let prev_acks = common.acks;

    // C parity (recGbl.c:188-189): clamp pending severity at INVALID_ALARM.
    // Records that erroneously call `recGblSetSevr` with a severity > 3
    // (e.g. via field-typed values that round-trip through u16) would
    // otherwise corrupt `sevr` into an undefined alarm severity. Keep the
    // existing nsev variant if it's already valid — only re-encode when
    // an out-of-range value snuck in.
    if (common.nsev as u16) > (AlarmSeverity::Invalid as u16) {
        common.nsev = AlarmSeverity::Invalid;
    }

    // Transfer new alarm state
    common.sevr = common.nsev;
    common.stat = common.nsta;
    common.amsg = std::mem::take(&mut common.namsg);

    // Reset for next cycle
    common.nsev = AlarmSeverity::NoAlarm;
    common.nsta = alarm_status::NO_ALARM;
    // common.namsg already cleared by `mem::take` above.

    let alarm_changed = common.sevr != prev_sevr || common.stat != prev_stat;
    let amsg_changed = common.amsg != prev_amsg;

    // C parity (recGbl.c:209-217): when an alarm-class field changed
    // this cycle, update the alarm-acknowledge severity `acks`. If
    // `ackt` is false (alarm is transient — automatically resets when
    // the condition clears) OR the new severity is >= the currently
    // remembered acks, raise `acks` to the new severity. Operators
    // clear `acks` back to NoAlarm via a CA put to ACKS (handled in
    // record_instance::put_common_field). Without this update, the
    // alarm-handler workflow (sticky severity tracking) is silently
    // disabled — every operator clear is a no-op because acks never
    // gets raised in the first place.
    let mut acks_changed = false;
    if alarm_changed || amsg_changed {
        if !common.ackt || (common.sevr as u16) >= (common.acks as u16) {
            if common.acks != common.sevr {
                common.acks = common.sevr;
                acks_changed = true;
            }
        }
    }

    let _ = prev_acks; // reserved for future post-event integration

    AlarmResetResult {
        alarm_changed,
        prev_sevr,
        prev_stat,
        amsg_changed,
        acks_changed,
    }
}

/// Check UDF alarm: if record is still undefined, raise UDF_ALARM with UDFS severity.
pub fn rec_gbl_check_udf(common: &mut CommonFields) {
    if common.udf {
        rec_gbl_set_sevr_msg(
            common,
            alarm_status::UDF_ALARM,
            common.udfs,
            "UDF: record not initialized",
        );
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
    fn test_check_udf_uses_udfs() {
        let mut common = CommonFields::default();
        assert!(common.udf);
        common.udfs = AlarmSeverity::Minor;
        rec_gbl_check_udf(&mut common);
        assert_eq!(common.nsev, AlarmSeverity::Minor);
        assert_eq!(common.nsta, alarm_status::UDF_ALARM);
    }

    #[test]
    fn test_check_udf_default_udfs_is_invalid() {
        let common = CommonFields::default();
        assert_eq!(common.udfs, AlarmSeverity::Invalid);
    }

    // ----- AMSG / NAMSG (epics-base PR #568 / #566) -----

    #[test]
    fn set_sevr_msg_writes_namsg_when_raised() {
        let mut common = CommonFields::default();
        rec_gbl_set_sevr_msg(
            &mut common,
            alarm_status::HIGH_ALARM,
            AlarmSeverity::Minor,
            "above HIGH threshold",
        );
        assert_eq!(common.nsev, AlarmSeverity::Minor);
        assert_eq!(common.nsta, alarm_status::HIGH_ALARM);
        assert_eq!(common.namsg, "above HIGH threshold");
    }

    #[test]
    fn set_sevr_msg_keeps_higher_message() {
        let mut common = CommonFields::default();
        rec_gbl_set_sevr_msg(
            &mut common,
            alarm_status::HIHI_ALARM,
            AlarmSeverity::Major,
            "above HIHI",
        );
        // Lower-severity follow-up must NOT overwrite the message.
        rec_gbl_set_sevr_msg(
            &mut common,
            alarm_status::HIGH_ALARM,
            AlarmSeverity::Minor,
            "would-be lower message",
        );
        assert_eq!(common.nsev, AlarmSeverity::Major);
        assert_eq!(common.namsg, "above HIHI");
    }

    #[test]
    fn reset_alarms_transfers_amsg_and_clears_namsg() {
        let mut common = CommonFields::default();
        rec_gbl_set_sevr_msg(
            &mut common,
            alarm_status::HIHI_ALARM,
            AlarmSeverity::Major,
            "above HIHI",
        );
        let result = rec_gbl_reset_alarms(&mut common);
        assert!(result.alarm_changed);
        assert!(result.amsg_changed);
        assert_eq!(common.amsg, "above HIHI");
        assert_eq!(common.namsg, "");
    }

    #[test]
    fn reset_alarms_clears_amsg_when_no_new_message() {
        let mut common = CommonFields::default();
        // First cycle: raise an alarm with message.
        rec_gbl_set_sevr_msg(
            &mut common,
            alarm_status::HIGH_ALARM,
            AlarmSeverity::Minor,
            "first cycle",
        );
        rec_gbl_reset_alarms(&mut common);
        assert_eq!(common.amsg, "first cycle");
        // Second cycle: no new alarm, no new message — amsg must clear.
        let result = rec_gbl_reset_alarms(&mut common);
        assert!(result.amsg_changed);
        assert_eq!(common.amsg, "");
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

    // ----- ACKS / ACKT auto-raise (recGbl.c:209-217 parity) -----

    /// C `recGblResetAlarms` raises `acks` to the new severity when the
    /// alarm changes, unless `ackt` is true AND the new severity is
    /// below the current `acks`. Default `ackt=true` is the "sticky"
    /// alarm-acknowledge mode — once raised, `acks` only drops when an
    /// operator writes back to ACKS. Without this, sticky-alarm
    /// tracking is dead on every record built with default ACKT=YES.
    #[test]
    fn reset_alarms_raises_acks_to_new_severity() {
        let mut common = CommonFields::default();
        assert_eq!(common.acks, AlarmSeverity::NoAlarm);
        assert!(common.ackt, "ACKT defaults to true (sticky)");

        rec_gbl_set_sevr(&mut common, alarm_status::HIHI_ALARM, AlarmSeverity::Major);
        let result = rec_gbl_reset_alarms(&mut common);

        assert!(result.alarm_changed);
        assert!(result.acks_changed, "first alarm raise must update acks");
        assert_eq!(common.acks, AlarmSeverity::Major);
    }

    /// `ackt=true` + dropping severity must NOT lower `acks` — the
    /// operator clears it via a CA put to ACKS. C path:
    /// `if (!ackt || new_sevr >= acks) acks = new_sevr` — when ackt=true
    /// AND new_sevr < acks, neither branch matches and acks stays.
    #[test]
    fn reset_alarms_keeps_acks_sticky_when_ackt_true_and_severity_drops() {
        let mut common = CommonFields::default();
        // First cycle: raise to MAJOR.
        rec_gbl_set_sevr(&mut common, alarm_status::HIHI_ALARM, AlarmSeverity::Major);
        rec_gbl_reset_alarms(&mut common);
        assert_eq!(common.acks, AlarmSeverity::Major);

        // Second cycle: drop to MINOR. acks must stay at MAJOR (sticky).
        rec_gbl_set_sevr(&mut common, alarm_status::HIGH_ALARM, AlarmSeverity::Minor);
        let result = rec_gbl_reset_alarms(&mut common);
        assert!(result.alarm_changed);
        assert!(
            !result.acks_changed,
            "ackt=true must NOT lower acks when severity drops"
        );
        assert_eq!(common.acks, AlarmSeverity::Major);
    }

    /// `ackt=false` (transient alarm acknowledge) drops `acks` together
    /// with severity. C path: `!ackt` short-circuits and assigns
    /// `acks = new_sevr` unconditionally on every alarm change.
    #[test]
    fn reset_alarms_drops_acks_when_ackt_false() {
        let mut common = CommonFields::default();
        common.ackt = false; // transient mode
        rec_gbl_set_sevr(&mut common, alarm_status::HIHI_ALARM, AlarmSeverity::Major);
        rec_gbl_reset_alarms(&mut common);
        assert_eq!(common.acks, AlarmSeverity::Major);

        rec_gbl_set_sevr(&mut common, alarm_status::HIGH_ALARM, AlarmSeverity::Minor);
        let result = rec_gbl_reset_alarms(&mut common);
        assert!(result.acks_changed);
        assert_eq!(
            common.acks,
            AlarmSeverity::Minor,
            "ackt=false drops acks with severity"
        );
    }
}
