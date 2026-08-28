use std::time::{SystemTime, UNIX_EPOCH};

/// EPICS epoch starts at 1990-01-01 00:00:00 UTC.
/// Equals pvxs/pvData `POSIX_TIME_AT_EPICS_EPOCH`; added when converting an
/// EPICS-epoch timestamp to the POSIX-epoch `time_t` carried on the wire.
pub const EPICS_EPOCH_OFFSET: u64 = 631_152_000;

/// Lightweight EPICS timestamp.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct EpicsTimestamp {
    pub sec: u32,
    pub nsec: u32,
}

impl EpicsTimestamp {
    pub fn now() -> Self {
        Self::from(SystemTime::now())
    }

    pub fn as_f64(&self) -> f64 {
        self.sec as f64 + self.nsec as f64 * 1e-9
    }
}

impl EpicsTimestamp {
    /// Convert back to `SystemTime`.
    pub fn to_system_time(&self) -> SystemTime {
        let unix_secs = self.sec as u64 + EPICS_EPOCH_OFFSET;
        UNIX_EPOCH + std::time::Duration::new(unix_secs, self.nsec)
    }
}

impl From<SystemTime> for EpicsTimestamp {
    /// Wrapping, not saturating: C assigns an `epicsInt64` difference into an
    /// unsigned `secPastEpoch` (`epicsTime.cpp:305-310` at `R7.0.10`), so a
    /// clock outside 1990-01-01 .. 2106-02-07 wraps modulo 2^32. Saturating
    /// pinned every reading from a board whose RTC never started to exactly
    /// `sec = 0`, which is a value a client cannot tell from a real stamp at
    /// the epoch — and which C never produces.
    fn from(st: SystemTime) -> Self {
        match st.duration_since(UNIX_EPOCH) {
            Ok(d) => Self {
                sec: d.as_secs().wrapping_sub(EPICS_EPOCH_OFFSET) as u32,
                nsec: d.subsec_nanos(),
            },
            // Before the Unix epoch is not a clock reading this port can carry
            // at all, so it stays the `{0, 0}` C uses for an uninitialized
            // `epicsTimeStamp` rather than becoming a wrapped instant.
            Err(_) => Self::default(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_now_nonzero() {
        let ts = EpicsTimestamp::now();
        assert!(ts.sec > 0);
    }

    #[test]
    fn test_as_f64() {
        let ts = EpicsTimestamp {
            sec: 100,
            nsec: 500_000_000,
        };
        assert!((ts.as_f64() - 100.5).abs() < 1e-9);
    }

    #[test]
    fn test_from_system_time() {
        use std::time::Duration;
        let st = UNIX_EPOCH + Duration::from_secs(EPICS_EPOCH_OFFSET + 1000);
        let ts = EpicsTimestamp::from(st);
        assert_eq!(ts.sec, 1000);
        assert_eq!(ts.nsec, 0);
    }

    #[test]
    fn pre_1990_wraps_exactly_as_c_wraps_it() {
        use std::time::Duration;
        // One second before the EPICS epoch: C computes -1 into an unsigned
        // `secPastEpoch` = 0xFFFF_FFFF. Saturating answered 0, which is what a
        // stamp taken exactly at the epoch answers.
        let st = UNIX_EPOCH + Duration::from_secs(EPICS_EPOCH_OFFSET - 1);
        assert_eq!(EpicsTimestamp::from(st).sec, u32::MAX);
    }

    #[test]
    fn the_epics_epoch_itself_is_still_zero() {
        use std::time::Duration;
        let st = UNIX_EPOCH + Duration::from_secs(EPICS_EPOCH_OFFSET);
        assert_eq!(EpicsTimestamp::from(st).sec, 0);
    }

    #[test]
    fn the_range_ends_wrap_and_do_not_pin() {
        use std::time::Duration;
        let last = UNIX_EPOCH + Duration::from_secs(EPICS_EPOCH_OFFSET + u32::MAX as u64);
        assert_eq!(EpicsTimestamp::from(last).sec, u32::MAX);
        let past = UNIX_EPOCH + Duration::from_secs(EPICS_EPOCH_OFFSET + u32::MAX as u64 + 1);
        assert_eq!(EpicsTimestamp::from(past).sec, 0);
    }

    #[test]
    fn test_default_is_zero() {
        let ts = EpicsTimestamp::default();
        assert_eq!(ts.sec, 0);
        assert_eq!(ts.nsec, 0);
    }
}
