use std::time::{SystemTime, UNIX_EPOCH};

/// EPICS epoch starts at 1990-01-01 00:00:00 UTC.
const EPICS_EPOCH_OFFSET: u64 = 631_152_000;

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

impl From<SystemTime> for EpicsTimestamp {
    fn from(st: SystemTime) -> Self {
        match st.duration_since(UNIX_EPOCH) {
            Ok(d) => {
                let unix_secs = d.as_secs();
                let epics_secs = unix_secs.saturating_sub(EPICS_EPOCH_OFFSET);
                Self {
                    sec: epics_secs as u32,
                    nsec: d.subsec_nanos(),
                }
            }
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
    fn test_default_is_zero() {
        let ts = EpicsTimestamp::default();
        assert_eq!(ts.sec, 0);
        assert_eq!(ts.nsec, 0);
    }
}
