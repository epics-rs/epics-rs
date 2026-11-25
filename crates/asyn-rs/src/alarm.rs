//! The record alarm an asyn status raises: C `asynStatusToEpicsAlarm`
//! (asynEpicsUtils.c:234-266). It builds without the `epics` feature, so the
//! port's own parameter reads share it with device support and downstream
//! drivers.

use crate::error::AsynStatus;

/// The `defaultStat` C's input device support passes.
pub const READ_ALARM: u16 = 1;
/// The `defaultStat` C's output device support passes.
pub const WRITE_ALARM: u16 = 2;

// The menuAlarmStat/menuAlarmSevr ordinals (libcom alarm.h:39-45,62-85),
// owned here so this module does not need `epics_base_rs`.
const NO_ALARM: u16 = 0;
const COMM_ALARM: u16 = 9;
const TIMEOUT_ALARM: u16 = 10;
const HW_LIMIT_ALARM: u16 = 11;
const DISABLE_ALARM: u16 = 18;
const SEV_NO_ALARM: u16 = 0;
const SEV_INVALID: u16 = 3;

/// C `asynStatusToEpicsAlarm`: fill each field of `alarm` (condition,
/// severity) that is still NO_ALARM from what `status` implies, with
/// `default_stat` as the condition for `asynError`. `asynSuccess` leaves
/// `alarm` as it is. The severity is INVALID, the `defaultSevr` every C
/// caller passes.
pub fn status_to_alarm(status: AsynStatus, default_stat: u16, alarm: (u16, u16)) -> (u16, u16) {
    let stat = match status {
        AsynStatus::Success => return alarm,
        AsynStatus::Timeout => TIMEOUT_ALARM,
        AsynStatus::Overflow => HW_LIMIT_ALARM,
        AsynStatus::Error => default_stat,
        AsynStatus::Disconnected => COMM_ALARM,
        AsynStatus::Disabled => DISABLE_ALARM,
    };
    (
        if alarm.0 == NO_ALARM { stat } else { alarm.0 },
        if alarm.1 == SEV_NO_ALARM {
            SEV_INVALID
        } else {
            alarm.1
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const CLEAN: (u16, u16) = (NO_ALARM, SEV_NO_ALARM);
    const STATE_ALARM: u16 = 7;
    const SEV_MAJOR: u16 = 2;
    const ALL: [AsynStatus; 6] = [
        AsynStatus::Success,
        AsynStatus::Timeout,
        AsynStatus::Overflow,
        AsynStatus::Error,
        AsynStatus::Disconnected,
        AsynStatus::Disabled,
    ];

    #[test]
    fn each_status_raises_its_own_condition_at_invalid() {
        let raised = ALL.map(|s| (s, status_to_alarm(s, READ_ALARM, CLEAN)));
        assert_eq!(
            raised,
            [
                (AsynStatus::Success, CLEAN),
                (AsynStatus::Timeout, (TIMEOUT_ALARM, SEV_INVALID)),
                (AsynStatus::Overflow, (HW_LIMIT_ALARM, SEV_INVALID)),
                (AsynStatus::Error, (READ_ALARM, SEV_INVALID)),
                (AsynStatus::Disconnected, (COMM_ALARM, SEV_INVALID)),
                (AsynStatus::Disabled, (DISABLE_ALARM, SEV_INVALID)),
            ]
        );
    }

    #[test]
    fn the_supplied_default_is_the_condition_of_asyn_error_alone() {
        for status in ALL {
            let read = status_to_alarm(status, READ_ALARM, CLEAN);
            let write = status_to_alarm(status, WRITE_ALARM, CLEAN);
            if status == AsynStatus::Error {
                assert_eq!((read.0, write.0), (READ_ALARM, WRITE_ALARM));
            } else {
                assert_eq!(read, write, "{status:?} takes no default");
            }
        }
    }

    #[test]
    fn a_field_already_set_is_kept_and_one_still_none_is_filled() {
        let stat_set = (STATE_ALARM, SEV_NO_ALARM);
        let sevr_set = (NO_ALARM, SEV_MAJOR);
        let both_set = (STATE_ALARM, SEV_MAJOR);
        for status in ALL {
            let fresh = status_to_alarm(status, READ_ALARM, CLEAN);
            assert_eq!(
                status_to_alarm(status, READ_ALARM, stat_set),
                (STATE_ALARM, fresh.1),
                "{status:?}"
            );
            assert_eq!(
                status_to_alarm(status, READ_ALARM, sevr_set),
                (fresh.0, SEV_MAJOR),
                "{status:?}"
            );
            assert_eq!(
                status_to_alarm(status, READ_ALARM, both_set),
                both_set,
                "{status:?}"
            );
        }
    }

    /// The owned codes are the record ABI, so they must equal base's.
    #[cfg(feature = "epics")]
    #[test]
    fn asyn_alarm_codes_match_epics_base() {
        use epics_base_rs::server::recgbl::alarm_status as base;
        use epics_base_rs::server::record::AlarmSeverity;
        assert_eq!(NO_ALARM, base::NO_ALARM);
        assert_eq!(READ_ALARM, base::READ_ALARM);
        assert_eq!(WRITE_ALARM, base::WRITE_ALARM);
        assert_eq!(COMM_ALARM, base::COMM_ALARM);
        assert_eq!(TIMEOUT_ALARM, base::TIMEOUT_ALARM);
        assert_eq!(HW_LIMIT_ALARM, base::HW_LIMIT_ALARM);
        assert_eq!(DISABLE_ALARM, base::DISABLE_ALARM);
        assert_eq!(STATE_ALARM, base::STATE_ALARM);
        assert_eq!(SEV_NO_ALARM, AlarmSeverity::NoAlarm as u16);
        assert_eq!(SEV_MAJOR, AlarmSeverity::Major as u16);
        assert_eq!(SEV_INVALID, AlarmSeverity::Invalid as u16);
    }

    /// C raises these conditions in `asynStatusToEpicsAlarm` and nowhere else
    /// in asyn, so another file of this crate naming one carries a copy of it.
    #[test]
    fn no_other_file_of_the_crate_maps_a_status_to_an_alarm() {
        use source_guard::{Comments, production, sweep};
        let mut copies = Vec::new();
        for (file, src) in sweep(concat!(env!("CARGO_MANIFEST_DIR"), "/src"), &["alarm.rs"]) {
            let prod = production(src, Comments::Strip);
            for needle in ["TIMEOUT_ALARM", "HW_LIMIT_ALARM", "DISABLE_ALARM"] {
                if prod.contains(needle) {
                    copies.push((file, needle));
                }
            }
        }
        assert_eq!(copies, [], "map an asyn status through `status_to_alarm`");
    }
}
