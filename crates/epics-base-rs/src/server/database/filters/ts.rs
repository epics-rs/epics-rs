//! `ts` — timestamp filter (epics-base 3.15.7 channel filters).
//!
//! Mirrors C `ts.c` (modules/database/src/std/filters/ts.c). Six
//! semantic modes:
//!
//! | mode             | JSON key/value           | output                                |
//! |------------------|--------------------------|---------------------------------------|
//! | `Generate`       | `{"ts":{}}`              | Rewrite snapshot.timestamp to now.    |
//! | `Double`         | `{"ts":{"num":"dbl"}}`   | Replace value with `f64` seconds.     |
//! | `Seconds`        | `{"ts":{"num":"sec"}}`   | Replace value with `i32` seconds.     |
//! | `Nanoseconds`    | `{"ts":{"num":"nsec"}}`  | Replace value with `i32` nanoseconds. |
//! | `Array`          | `{"ts":{"num":"ts"}}`    | Replace with `LongArray[sec, nsec]`.  |
//! | `String`         | `{"ts":{"str":"epics"}}` | Replace with formatted string.        |
//!
//! Epoch options (Numeric / Array modes only):
//! * `epoch=epics` (default) — seconds since 1990-01-01.
//! * `epoch=unix` — seconds since 1970-01-01 (adds the 631_152_000s
//!   POSIX offset to the EPICS-epoch base).
//!
//! C `ts.c::filter` short-circuits `DBE_PROPERTY` and read-context;
//! the Rust port matches that on the timestamp mode (Generate) —
//! the value-replacement modes are transformations that should run
//! on every emission.

use super::{FilteredMonitorEvent, SubscriptionFilter};
use crate::types::EpicsValue;

/// EPICS epoch (1990-01-01 00:00:00 UTC) offset from the Unix epoch.
/// Identical to `EPICS_UNIX_EPOCH_OFFSET_SECS` in `types/codec.rs`;
/// re-declared here so the filter doesn't reach into a private
/// module.
const EPICS_UNIX_EPOCH_OFFSET_SECS: u64 = 631_152_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TsMode {
    /// Default: rewrite `snapshot.timestamp` to the current wall
    /// clock; value is left untouched.
    Generate,
    /// Replace value with `sec + nsec * 1e-9` (Double).
    Double,
    /// Replace value with `sec` (Long).
    Seconds,
    /// Replace value with `nsec` (Long).
    Nanoseconds,
    /// Replace value with a 2-element `LongArray = [sec, nsec]`.
    Array,
    /// Replace value with the EPICS-format timestamp string
    /// (`%Y-%m-%d %H:%M:%S.%06f`).
    StringEpics,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TsEpoch {
    /// Seconds-since-EPICS-epoch (1990-01-01).
    Epics,
    /// Seconds-since-Unix-epoch (1970-01-01).
    Unix,
}

pub struct TimestampFilter {
    mode: TsMode,
    epoch: TsEpoch,
}

impl TimestampFilter {
    /// Default mode = `Generate` (matches C `tsModeGenerate`).
    pub fn new() -> Self {
        Self {
            mode: TsMode::Generate,
            epoch: TsEpoch::Epics,
        }
    }

    pub fn with_mode(mode: TsMode) -> Self {
        Self {
            mode,
            epoch: TsEpoch::Epics,
        }
    }

    pub fn with_mode_epoch(mode: TsMode, epoch: TsEpoch) -> Self {
        Self { mode, epoch }
    }
}

impl Default for TimestampFilter {
    fn default() -> Self {
        Self::new()
    }
}

/// Extract `(sec, nsec)` from a `SystemTime`, optionally offset by
/// the EPICS→Unix epoch difference per `epoch`.
fn ts_parts(t: std::time::SystemTime, epoch: TsEpoch) -> (i64, u32) {
    let unix = t
        .duration_since(std::time::SystemTime::UNIX_EPOCH)
        .unwrap_or(std::time::Duration::ZERO);
    let sec_unix = unix.as_secs() as i64;
    let nsec = unix.subsec_nanos();
    let sec = match epoch {
        TsEpoch::Unix => sec_unix,
        TsEpoch::Epics => sec_unix - EPICS_UNIX_EPOCH_OFFSET_SECS as i64,
    };
    (sec, nsec)
}

fn format_epics_string(t: std::time::SystemTime) -> String {
    use chrono::{DateTime, Utc};
    let unix_secs = t
        .duration_since(std::time::SystemTime::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0);
    let dt: DateTime<Utc> =
        DateTime::from_timestamp(unix_secs.trunc() as i64, ((unix_secs.fract()) * 1e9) as u32)
            .unwrap_or_else(Utc::now);
    dt.format("%Y-%m-%d %H:%M:%S%.6f").to_string()
}

impl SubscriptionFilter for TimestampFilter {
    fn name(&self) -> &'static str {
        "ts"
    }

    fn apply(&self, mut event: FilteredMonitorEvent) -> Option<FilteredMonitorEvent> {
        match self.mode {
            TsMode::Generate => {
                // Replace the snapshot timestamp with "now" — what
                // the original C `generate()` does.
                event.event.snapshot.timestamp = crate::runtime::time::now_wall();
            }
            TsMode::Double => {
                let (sec, nsec) = ts_parts(event.event.snapshot.timestamp, self.epoch);
                let v = sec as f64 + (nsec as f64) * 1e-9;
                event.event.snapshot.value = EpicsValue::Double(v);
            }
            TsMode::Seconds => {
                // C `ts.c:199` sets `field_type = DBF_ULONG` and
                // writes an `epicsUInt32`. The Rust value model has
                // no unsigned-32 scalar; `Int64` carries the full
                // `epicsUInt32` range losslessly so a Unix-epoch
                // `sec` past `i32::MAX` (year 2038) is not truncated.
                let (sec, _) = ts_parts(event.event.snapshot.timestamp, self.epoch);
                event.event.snapshot.value = EpicsValue::Int64(sec);
            }
            TsMode::Nanoseconds => {
                // Nanoseconds are always < 1e9 — fits any width;
                // kept as `Int64` for type-consistency with the
                // seconds / array modes.
                let (_, nsec) = ts_parts(event.event.snapshot.timestamp, self.epoch);
                event.event.snapshot.value = EpicsValue::Int64(nsec as i64);
            }
            TsMode::Array => {
                // C `ts_array` produces a 2-element `DBF_ULONG`
                // array. `Int64Array` holds the unsigned-32 range
                // without the post-2038 seconds truncation.
                let (sec, nsec) = ts_parts(event.event.snapshot.timestamp, self.epoch);
                event.event.snapshot.value = EpicsValue::Int64Array(vec![sec, nsec as i64]);
            }
            TsMode::StringEpics => {
                event.event.snapshot.value =
                    EpicsValue::String(format_epics_string(event.event.snapshot.timestamp).into());
            }
        }
        Some(event)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::pv::MonitorEvent;
    use crate::server::recgbl::EventMask;
    use crate::server::snapshot::Snapshot;
    use crate::types::EpicsValue;
    use std::time::{Duration, SystemTime};

    fn make_event(t: SystemTime) -> FilteredMonitorEvent {
        FilteredMonitorEvent::new(
            MonitorEvent {
                snapshot: Snapshot::new(EpicsValue::Double(1.0), 0, 0, t),
                origin: 0,
            },
            EventMask::VALUE,
        )
    }

    /// The default `Generate` mode rewrites snapshot timestamp to "now".
    #[test]
    fn rewrites_snapshot_timestamp_to_now() {
        let f = TimestampFilter::new();
        let before = SystemTime::now();
        let out = f.apply(make_event(SystemTime::UNIX_EPOCH)).unwrap();
        let stamped = out.event.snapshot.timestamp;
        assert!(
            stamped >= before - Duration::from_millis(1),
            "stamp must reflect current wall clock"
        );
    }

    /// Filter never drops an event.
    #[test]
    fn never_drops_an_event() {
        let f = TimestampFilter::new();
        assert!(f.apply(make_event(SystemTime::UNIX_EPOCH)).is_some());
    }

    /// Default Generate also re-stamps alarm-only emissions.
    #[test]
    fn restamps_alarm_events_too() {
        let f = TimestampFilter::new();
        let mut ev = make_event(SystemTime::UNIX_EPOCH);
        ev.mask = EventMask::ALARM;
        let out = f.apply(ev).unwrap();
        assert!(out.event.snapshot.timestamp > SystemTime::UNIX_EPOCH);
    }

    /// `num=sec` with `epoch=epics`: a wall time at EPICS-epoch+10s
    /// surfaces as `10`.
    #[test]
    fn sec_mode_epics_epoch() {
        let ts = SystemTime::UNIX_EPOCH + Duration::from_secs(EPICS_UNIX_EPOCH_OFFSET_SECS + 10);
        let f = TimestampFilter::with_mode_epoch(TsMode::Seconds, TsEpoch::Epics);
        let out = f.apply(make_event(ts)).unwrap();
        match out.event.snapshot.value {
            // C `ts.c` uses DBF_ULONG — modelled as Int64 (no
            // unsigned-32 scalar in the Rust value model).
            EpicsValue::Int64(v) => assert_eq!(v, 10),
            other => panic!("expected Int64(10), got {other:?}"),
        }
    }

    /// `num=sec` with `epoch=unix`: the same wall time surfaces as
    /// the full Unix-seconds value (no offset subtraction).
    #[test]
    fn sec_mode_unix_epoch() {
        let ts = SystemTime::UNIX_EPOCH + Duration::from_secs(EPICS_UNIX_EPOCH_OFFSET_SECS + 10);
        let f = TimestampFilter::with_mode_epoch(TsMode::Seconds, TsEpoch::Unix);
        let out = f.apply(make_event(ts)).unwrap();
        match out.event.snapshot.value {
            EpicsValue::Int64(v) => assert_eq!(v as u64, EPICS_UNIX_EPOCH_OFFSET_SECS + 10),
            other => panic!("expected Int64, got {other:?}"),
        }
    }

    /// `num=nsec` extracts the sub-second nanoseconds (epoch-independent).
    #[test]
    fn nsec_mode_extracts_subsecond_only() {
        let ts = SystemTime::UNIX_EPOCH + Duration::new(EPICS_UNIX_EPOCH_OFFSET_SECS, 123_456_789);
        let f = TimestampFilter::with_mode(TsMode::Nanoseconds);
        let out = f.apply(make_event(ts)).unwrap();
        match out.event.snapshot.value {
            EpicsValue::Int64(v) => assert_eq!(v, 123_456_789),
            other => panic!("expected Int64(123456789), got {other:?}"),
        }
    }

    /// `num=dbl` returns `sec + nsec * 1e-9` (Double).
    #[test]
    fn double_mode_combines_sec_and_nsec() {
        let ts =
            SystemTime::UNIX_EPOCH + Duration::new(EPICS_UNIX_EPOCH_OFFSET_SECS + 5, 250_000_000);
        let f = TimestampFilter::with_mode_epoch(TsMode::Double, TsEpoch::Epics);
        let out = f.apply(make_event(ts)).unwrap();
        match out.event.snapshot.value {
            EpicsValue::Double(v) => {
                assert!((v - 5.25).abs() < 1e-9, "expected 5.25, got {v}");
            }
            other => panic!("expected Double, got {other:?}"),
        }
    }

    /// `num=ts` returns an `Int64Array[sec, nsec]` (C DBF_ULONG pair).
    #[test]
    fn array_mode_returns_sec_nsec_pair() {
        let ts =
            SystemTime::UNIX_EPOCH + Duration::new(EPICS_UNIX_EPOCH_OFFSET_SECS + 7, 500_000_000);
        let f = TimestampFilter::with_mode(TsMode::Array);
        let out = f.apply(make_event(ts)).unwrap();
        match out.event.snapshot.value {
            EpicsValue::Int64Array(v) => {
                assert_eq!(v, vec![7, 500_000_000]);
            }
            other => panic!("expected Int64Array, got {other:?}"),
        }
    }

    /// Post-2038 regression: a Unix-epoch `sec` value beyond
    /// `i32::MAX` must NOT be truncated. C `ts.c` uses the full
    /// `epicsUInt32` range; the Rust port keeps it lossless in
    /// `Int64`.
    #[test]
    fn sec_mode_unix_epoch_no_post_2038_truncation() {
        // 2^31 + 1000 seconds past the Unix epoch — beyond i32::MAX.
        let big = i32::MAX as u64 + 1000;
        let ts = SystemTime::UNIX_EPOCH + Duration::from_secs(EPICS_UNIX_EPOCH_OFFSET_SECS + big);
        let f = TimestampFilter::with_mode_epoch(TsMode::Seconds, TsEpoch::Unix);
        let out = f.apply(make_event(ts)).unwrap();
        match out.event.snapshot.value {
            EpicsValue::Int64(v) => {
                assert_eq!(v as u64, EPICS_UNIX_EPOCH_OFFSET_SECS + big);
                assert!(v > i32::MAX as i64, "post-2038 value not truncated");
            }
            other => panic!("expected Int64, got {other:?}"),
        }
    }

    /// `str=epics` returns a formatted `YYYY-MM-DD HH:MM:SS.ffffff` string.
    #[test]
    fn string_mode_formats_epics_style() {
        // 2024-01-15 12:34:56.123456 UTC
        let secs_since_unix = 1_705_322_096_u64;
        let ts = SystemTime::UNIX_EPOCH + Duration::new(secs_since_unix, 123_456_000);
        let f = TimestampFilter::with_mode(TsMode::StringEpics);
        let out = f.apply(make_event(ts)).unwrap();
        match out.event.snapshot.value {
            EpicsValue::String(s) => {
                // Sanity: contains the date and the microsecond fraction.
                let s = s.as_str_lossy();
                assert!(
                    s.starts_with("2024-01-15") && s.contains(".123456"),
                    "unexpected format: {s}"
                );
            }
            other => panic!("expected String, got {other:?}"),
        }
    }
}
