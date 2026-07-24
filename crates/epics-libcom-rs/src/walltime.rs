//! [`WallTime`] — a wall-clock instant with full EPICS nanosecond precision
//! on every platform.

use std::time::{Duration, SystemTime};

/// Wall-clock instant carrying the full EPICS `nsec` (1 ns precision) on every
/// platform.
///
/// Stored as a [`Duration`] since the Unix epoch (integer seconds + nanos), so
/// a wire-sourced `epicsTimeStamp.nsec` survives a store→serve round-trip
/// exactly. [`std::time::SystemTime`] is backed by `FILETIME` on Windows and
/// only resolves to 100 ns, which truncated the low nanosecond digits of an
/// externally supplied timestamp (a PVA PUT, a gateway pass-through, a
/// `Q:time:tag` nsec split); `WallTime` keeps integers end-to-end and never
/// truncates. On Linux/macOS `SystemTime` already holds 1 ns, so this only
/// changes behaviour on Windows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct WallTime {
    since_epoch: Duration,
}

impl WallTime {
    /// The Unix epoch (1970-01-01T00:00:00Z) — the zero instant.
    pub const UNIX_EPOCH: Self = Self {
        since_epoch: Duration::ZERO,
    };

    /// Current wall-clock time from the OS clock.
    ///
    /// On Windows the OS clock itself is 100 ns-granular (the same limit C
    /// EPICS hits), so "now" loses no precision relative to C; only an
    /// externally supplied sub-100 ns `nsec` would, and that arrives through
    /// [`WallTime::from_unix`].
    pub fn now() -> Self {
        SystemTime::now().into()
    }

    /// Build from an integer `(unix_seconds, nanoseconds)` pair. `nanos` at or
    /// above 1e9 carries into seconds, matching [`Duration::new`].
    pub fn from_unix(secs: u64, nanos: u32) -> Self {
        Self {
            since_epoch: Duration::new(secs, nanos),
        }
    }

    /// Time elapsed since the Unix epoch as a [`Duration`] (1 ns precision).
    pub fn since_unix_epoch(self) -> Duration {
        self.since_epoch
    }

    /// Whole seconds since the Unix epoch.
    pub fn unix_secs(self) -> u64 {
        self.since_epoch.as_secs()
    }

    /// Sub-second nanoseconds in `0..1_000_000_000`.
    pub fn subsec_nanos(self) -> u32 {
        self.since_epoch.subsec_nanos()
    }

    /// This instant minus `d`, saturating at [`WallTime::UNIX_EPOCH`] rather
    /// than panicking on underflow.
    pub fn saturating_sub(self, d: Duration) -> Self {
        Self {
            since_epoch: self.since_epoch.saturating_sub(d),
        }
    }
}

impl From<SystemTime> for WallTime {
    /// Times before the Unix epoch clamp to [`WallTime::UNIX_EPOCH`]
    /// (`unwrap_or_default`), matching the previous
    /// `duration_since(UNIX_EPOCH).unwrap_or(ZERO)` behaviour at the
    /// snapshot/codec boundaries.
    fn from(t: SystemTime) -> Self {
        Self {
            since_epoch: t.duration_since(SystemTime::UNIX_EPOCH).unwrap_or_default(),
        }
    }
}

impl From<WallTime> for SystemTime {
    /// For display/formatting interop (e.g. `chrono`). On Windows this
    /// re-introduces the platform's 100 ns `SystemTime` granularity, so use
    /// the integer accessors ([`WallTime::unix_secs`] / [`WallTime::subsec_nanos`])
    /// when the full `nsec` must be preserved.
    fn from(t: WallTime) -> Self {
        SystemTime::UNIX_EPOCH + t.since_epoch
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_unix_preserves_full_nanoseconds() {
        // The exact value that std SystemTime truncates to 100 ns on Windows.
        let t = WallTime::from_unix(42, 123_456_789);
        assert_eq!(t.unix_secs(), 42);
        assert_eq!(t.subsec_nanos(), 123_456_789);
        assert_eq!(t.since_unix_epoch(), Duration::new(42, 123_456_789));
    }

    #[test]
    fn systemtime_round_trip_through_unix_epoch() {
        let st = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        let wt: WallTime = st.into();
        assert_eq!(wt.unix_secs(), 1_700_000_000);
        let back: SystemTime = wt.into();
        assert_eq!(back, st);
    }

    #[test]
    fn pre_epoch_systemtime_clamps_to_epoch() {
        let before = SystemTime::UNIX_EPOCH - Duration::from_secs(5);
        let wt: WallTime = before.into();
        assert_eq!(wt, WallTime::UNIX_EPOCH);
    }

    #[test]
    fn saturating_sub_floors_at_epoch() {
        let t = WallTime::from_unix(0, 500);
        assert_eq!(
            t.saturating_sub(Duration::from_secs(1)),
            WallTime::UNIX_EPOCH
        );
        assert_eq!(
            WallTime::from_unix(10, 0).saturating_sub(Duration::from_secs(3)),
            WallTime::from_unix(7, 0)
        );
    }

    #[test]
    fn ordering_matches_chronology() {
        assert!(WallTime::from_unix(1, 0) < WallTime::from_unix(1, 1));
        assert!(WallTime::from_unix(2, 0) > WallTime::from_unix(1, 999_999_999));
        assert_eq!(WallTime::UNIX_EPOCH, WallTime::from_unix(0, 0));
    }
}
