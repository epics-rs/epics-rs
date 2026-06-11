use std::time::{Duration, Instant, SystemTime};

use crate::types::WallTime;

/// Current wall-clock time as a [`WallTime`].
///
/// Returns [`WallTime`] rather than [`SystemTime`] so a snapshot built from
/// "now" shares one timestamp type with snapshots built from exact wire
/// integers. The OS clock is still read via [`SystemTime::now`]; on Windows
/// that clock is itself 100 ns-granular, which `WallTime` does not change.
pub fn now_wall() -> WallTime {
    SystemTime::now().into()
}

pub fn now_mono() -> Instant {
    Instant::now()
}

pub fn deadline_from_now(d: Duration) -> Instant {
    Instant::now() + d
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_now_wall() {
        let t = now_wall();
        assert!(t.since_unix_epoch().as_secs() > 0);
    }

    #[test]
    fn test_now_mono() {
        let t1 = now_mono();
        let t2 = now_mono();
        assert!(t2 >= t1);
    }

    #[test]
    fn test_deadline_from_now() {
        let before = Instant::now();
        let deadline = deadline_from_now(Duration::from_secs(10));
        assert!(deadline > before);
        assert!(deadline <= before + Duration::from_secs(11));
    }
}
