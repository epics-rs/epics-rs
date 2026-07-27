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
use crate::types::{EpicsValue, WallTime};

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
    /// (`%Y-%m-%d %H:%M:%S.%06f`, C `tsStringEpics`).
    StringEpics,
    /// Replace value with the ISO-8601 timestamp string in LOCAL time
    /// (`%Y-%m-%dT%H:%M:%S.%06f%z`, C `tsStringIso`, ts.c:250) — a `T`
    /// separator and a `%z` zone offset, distinct from `StringEpics`.
    StringIso,
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

/// Extract `(sec, nsec)` from a [`WallTime`], optionally offset by
/// the EPICS→Unix epoch difference per `epoch`.
fn ts_parts(t: WallTime, epoch: TsEpoch) -> (i64, u32) {
    let unix = t.since_unix_epoch();
    let sec_unix = unix.as_secs() as i64;
    let nsec = unix.subsec_nanos();
    let sec = match epoch {
        TsEpoch::Unix => sec_unix,
        TsEpoch::Epics => sec_unix - EPICS_UNIX_EPOCH_OFFSET_SECS as i64,
    };
    (sec, nsec)
}

fn format_epics_string(t: WallTime) -> String {
    use chrono::{DateTime, Local, Utc};
    let unix_secs = t.since_unix_epoch().as_secs_f64();
    let utc: DateTime<Utc> =
        DateTime::from_timestamp(unix_secs.trunc() as i64, ((unix_secs.fract()) * 1e9) as u32)
            .unwrap_or_else(Utc::now);
    // C `tsStringEpics` formats through `epicsTimeToStrftime`, which
    // converts via `epicsTime_localtime` -> `localtime_r` (ts.c:247,
    // epicsTime.cpp:202 -> :318, osdTime.cpp:82): LOCAL wall-clock, same as
    // `tsStringIso`. The two modes differ ONLY by the separator (space vs
    // `T`) and the zoneless layout (no `%z`), never by timezone — so this
    // mirrors `format_iso_string`'s `with_timezone(&Local)` rather than
    // formatting UTC directly.
    utc.with_timezone(&Local)
        .format("%Y-%m-%d %H:%M:%S%.6f")
        .to_string()
}

fn format_iso_string(t: WallTime) -> String {
    use chrono::{DateTime, Local, Utc};
    let unix_secs = t.since_unix_epoch().as_secs_f64();
    let utc: DateTime<Utc> =
        DateTime::from_timestamp(unix_secs.trunc() as i64, ((unix_secs.fract()) * 1e9) as u32)
            .unwrap_or_else(Utc::now);
    // C `tsStringIso` formats in LOCAL time (epicsTimeToStrftime ->
    // epicsTime_localtime), so `%z` carries the real local-zone offset
    // (ts.c:250). The `T` separator and `%z` suffix distinguish it from
    // the space-separated, zoneless epics format (ts.c:247).
    utc.with_timezone(&Local)
        .format("%Y-%m-%dT%H:%M:%S%.6f%z")
        .to_string()
}

impl SubscriptionFilter for TimestampFilter {
    fn name(&self) -> &'static str {
        "ts"
    }

    fn apply(&self, mut event: FilteredMonitorEvent) -> Option<FilteredMonitorEvent> {
        // One `make_mut` for the whole filter: it copies the snapshot only
        // when another subscriber still holds the same `Arc`, and every arm
        // below writes through the same unique reference.
        let snap = std::sync::Arc::make_mut(&mut event.event.snapshot);
        match self.mode {
            TsMode::Generate => {
                // Replace the snapshot timestamp with "now" — what
                // the original C `generate()` does.
                snap.timestamp = crate::runtime::time::now_wall();
            }
            TsMode::Double => {
                let (sec, nsec) = ts_parts(snap.timestamp, self.epoch);
                let v = sec as f64 + (nsec as f64) * 1e-9;
                snap.value = EpicsValue::Double(v);
            }
            TsMode::Seconds => {
                // C `ts_seconds` (ts.c:196-203) sets `field_type =
                // DBF_ULONG` and writes the `epicsUInt32` `secPastEpoch`
                // (+POSIX offset for the Unix epoch). `sec as u32` matches
                // C's `epicsUInt32` storage — it wraps mod 2^32 (year
                // 2106), exactly as C does.
                let (sec, _) = ts_parts(snap.timestamp, self.epoch);
                snap.value = EpicsValue::ULong(sec as u32);
            }
            TsMode::Nanoseconds => {
                // C `ts_nanos` (ts.c:205-212) sets `DBF_ULONG` and writes
                // the `epicsUInt32` `nsec` (always < 1e9).
                let (_, nsec) = ts_parts(snap.timestamp, self.epoch);
                snap.value = EpicsValue::ULong(nsec);
            }
            TsMode::Array => {
                // C `ts_array` (ts.c:223-235) produces a 2-element
                // `DBF_ULONG` array `[secPastEpoch, nsec]` of `epicsUInt32`.
                let (sec, nsec) = ts_parts(snap.timestamp, self.epoch);
                snap.value = EpicsValue::ULongArray(vec![sec as u32, nsec]);
            }
            TsMode::StringEpics => {
                snap.value = EpicsValue::String(format_epics_string(snap.timestamp).into());
            }
            TsMode::StringIso => {
                snap.value = EpicsValue::String(format_iso_string(snap.timestamp).into());
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

    fn make_event(t: impl Into<WallTime>) -> FilteredMonitorEvent {
        FilteredMonitorEvent::new(MonitorEvent {
            snapshot: std::sync::Arc::new(Snapshot::new(EpicsValue::Double(1.0), 0, 0, t)),
            origin: 0,
            mask: EventMask::VALUE,
        })
    }

    /// The default `Generate` mode rewrites snapshot timestamp to "now".
    #[test]
    fn rewrites_snapshot_timestamp_to_now() {
        let f = TimestampFilter::new();
        let before: WallTime = SystemTime::now().into();
        let out = f.apply(make_event(SystemTime::UNIX_EPOCH)).unwrap();
        let stamped = out.event.snapshot.timestamp;
        assert!(
            stamped >= before.saturating_sub(Duration::from_millis(1)),
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
        ev.event.mask = EventMask::ALARM;
        let out = f.apply(ev).unwrap();
        assert!(out.event.snapshot.timestamp > WallTime::UNIX_EPOCH);
    }

    /// `num=sec` with `epoch=epics`: a wall time at EPICS-epoch+10s
    /// surfaces as `10`.
    #[test]
    fn sec_mode_epics_epoch() {
        let ts = SystemTime::UNIX_EPOCH + Duration::from_secs(EPICS_UNIX_EPOCH_OFFSET_SECS + 10);
        let f = TimestampFilter::with_mode_epoch(TsMode::Seconds, TsEpoch::Epics);
        let out = f.apply(make_event(ts)).unwrap();
        match out.event.snapshot.value.clone() {
            // C `ts_seconds` sets DBF_ULONG (epicsUInt32).
            EpicsValue::ULong(v) => assert_eq!(v, 10),
            other => panic!("expected ULong(10), got {other:?}"),
        }
    }

    /// `num=sec` with `epoch=unix`: the same wall time surfaces as
    /// the full Unix-seconds value (no offset subtraction).
    #[test]
    fn sec_mode_unix_epoch() {
        let ts = SystemTime::UNIX_EPOCH + Duration::from_secs(EPICS_UNIX_EPOCH_OFFSET_SECS + 10);
        let f = TimestampFilter::with_mode_epoch(TsMode::Seconds, TsEpoch::Unix);
        let out = f.apply(make_event(ts)).unwrap();
        match out.event.snapshot.value.clone() {
            EpicsValue::ULong(v) => assert_eq!(v as u64, EPICS_UNIX_EPOCH_OFFSET_SECS + 10),
            other => panic!("expected ULong, got {other:?}"),
        }
    }

    /// `num=nsec` extracts the sub-second nanoseconds (epoch-independent).
    #[test]
    fn nsec_mode_extracts_subsecond_only() {
        // Exact integer (secs, nsec): a `SystemTime` rounds 123_456_789 to
        // 100 ns on Windows, so this nsec must be injected as a `WallTime`.
        let ts = WallTime::from_unix(EPICS_UNIX_EPOCH_OFFSET_SECS, 123_456_789);
        let f = TimestampFilter::with_mode(TsMode::Nanoseconds);
        let out = f.apply(make_event(ts)).unwrap();
        match out.event.snapshot.value.clone() {
            EpicsValue::ULong(v) => assert_eq!(v, 123_456_789),
            other => panic!("expected ULong(123456789), got {other:?}"),
        }
    }

    /// `num=dbl` returns `sec + nsec * 1e-9` (Double).
    #[test]
    fn double_mode_combines_sec_and_nsec() {
        let ts = WallTime::from_unix(EPICS_UNIX_EPOCH_OFFSET_SECS + 5, 250_000_000);
        let f = TimestampFilter::with_mode_epoch(TsMode::Double, TsEpoch::Epics);
        let out = f.apply(make_event(ts)).unwrap();
        match out.event.snapshot.value.clone() {
            EpicsValue::Double(v) => {
                assert!((v - 5.25).abs() < 1e-9, "expected 5.25, got {v}");
            }
            other => panic!("expected Double, got {other:?}"),
        }
    }

    /// `num=ts` returns a `ULongArray[sec, nsec]` (C DBF_ULONG pair).
    #[test]
    fn array_mode_returns_sec_nsec_pair() {
        let ts = WallTime::from_unix(EPICS_UNIX_EPOCH_OFFSET_SECS + 7, 500_000_000);
        let f = TimestampFilter::with_mode(TsMode::Array);
        let out = f.apply(make_event(ts)).unwrap();
        match out.event.snapshot.value.clone() {
            EpicsValue::ULongArray(v) => {
                assert_eq!(v, vec![7u32, 500_000_000]);
            }
            other => panic!("expected ULongArray, got {other:?}"),
        }
    }

    /// Post-2038 regression: a Unix-epoch `sec` value beyond `i32::MAX`
    /// must NOT be truncated to a signed 32-bit field. C `ts.c` uses the
    /// full `epicsUInt32` range (wraps only at 2^32 ≈ year 2106); the
    /// Rust port serves the unsigned 32-bit value in `ULong`.
    #[test]
    fn sec_mode_unix_epoch_no_post_2038_truncation() {
        // 2^31 + 1000 seconds past the Unix epoch — beyond i32::MAX but
        // still within the epicsUInt32 range.
        let big = i32::MAX as u64 + 1000;
        let ts = SystemTime::UNIX_EPOCH + Duration::from_secs(EPICS_UNIX_EPOCH_OFFSET_SECS + big);
        let f = TimestampFilter::with_mode_epoch(TsMode::Seconds, TsEpoch::Unix);
        let out = f.apply(make_event(ts)).unwrap();
        match out.event.snapshot.value.clone() {
            EpicsValue::ULong(v) => {
                assert_eq!(v as u64, EPICS_UNIX_EPOCH_OFFSET_SECS + big);
                assert!(v > i32::MAX as u32, "post-2038 value not truncated");
            }
            other => panic!("expected ULong, got {other:?}"),
        }
    }

    /// `str=epics` returns a formatted `YYYY-MM-DD HH:MM:SS.ffffff` string.
    /// Timezone-independent structural assertions: `format_epics_string`
    /// now renders LOCAL wall-clock (C `epicsTimeToStrftime`), so the date
    /// and time fields depend on the host zone — pin only the shape
    /// (`\d{4}-\d\d-\d\d \d\d:\d\d:\d\d.123456`, zoneless, no `T`), not a
    /// UTC-derived date.
    #[test]
    fn string_mode_formats_epics_style() {
        // 2024-01-15 12:34:56.123456 UTC (local wall-clock varies by zone).
        let secs_since_unix = 1_705_322_096_u64;
        let ts = SystemTime::UNIX_EPOCH + Duration::new(secs_since_unix, 123_456_000);
        let f = TimestampFilter::with_mode(TsMode::StringEpics);
        let out = f.apply(make_event(ts)).unwrap();
        match out.event.snapshot.value.clone() {
            EpicsValue::String(s) => {
                let s = s.as_str_lossy();
                assert!(s.ends_with(".123456"), "must keep microseconds: {s}");
                assert!(
                    !s.contains('T'),
                    "epics format must not use a T separator: {s}"
                );
                let bytes = s.as_bytes();
                // "YYYY-MM-DD HH:MM:SS.ffffff" — digits/separators at fixed
                // offsets, regardless of the host timezone.
                assert_eq!(s.len(), 26, "unexpected length: {s}");
                for (i, c) in s.char_indices() {
                    let ok = match i {
                        4 | 7 => c == '-',
                        10 => c == ' ',
                        13 | 16 => c == ':',
                        19 => c == '.',
                        _ => c.is_ascii_digit(),
                    };
                    assert!(ok, "char {i} ({c:?}) breaks the epics layout: {s}");
                }
                let _ = bytes;
            }
            other => panic!("expected String, got {other:?}"),
        }
    }

    /// C `tsStringEpics` and `tsStringIso` BOTH format through
    /// `epicsTimeToStrftime` -> `epicsTime_localtime` -> `localtime_r`
    /// (ts.c:247/250, epicsTime.cpp:202 -> :318, osdTime.cpp:82): LOCAL
    /// wall-clock. The two layouts differ ONLY by separator (space vs `T`)
    /// and the ISO `%z` zone suffix — never by timezone. Earlier the epics
    /// path formatted UTC, diverging from C in any non-UTC host zone.
    #[test]
    fn string_epics_and_iso_share_local_wallclock() {
        // Force a fixed non-UTC zone (UTC+9, no DST) so the divergence is
        // deterministic regardless of the host zone. nextest runs each test
        // in its own process, so this `set_var` is read fresh before the
        // first `chrono::Local` conversion in this process. POSIX `XXX-9`
        // needs no tzdata files.
        unsafe { std::env::set_var("TZ", "XXX-9") }

        // 2024-01-15 12:34:56.123456 UTC == 2024-01-15 21:34:56.123456 +0900
        let ts = SystemTime::UNIX_EPOCH + Duration::new(1_705_322_096, 123_456_000);
        let extract = |mode| match TimestampFilter::with_mode(mode)
            .apply(make_event(ts))
            .unwrap()
            .event
            .snapshot
            .value
            .clone()
        {
            EpicsValue::String(s) => s.as_str_lossy().into_owned(),
            other => panic!("expected String, got {other:?}"),
        };
        let epics = extract(TsMode::StringEpics);
        let iso = extract(TsMode::StringIso);

        // iso normalized to the epics layout: T -> space, drop 5-char %z.
        let iso_norm = iso.replace('T', " ");
        let iso_norm = &iso_norm[..iso_norm.len() - 5];
        assert_eq!(
            epics, iso_norm,
            "epics and iso string modes must render the SAME local wall-clock"
        );

        // When the host honored TZ=UTC+9 (nextest process isolation), the
        // local wall-clock is the UTC instant + 9h. This deterministically
        // catches a UTC-formatting regression even on a UTC CI host.
        if iso.ends_with("+0900") {
            assert!(
                epics.starts_with("2024-01-15 21:34:56.123456"),
                "epics must show the +09:00 local wall-clock, got {epics}"
            );
        }
    }

    /// C `tsStringIso` (ts.c:250) emits a distinct ISO-8601 string in
    /// local time: a `T` date/time separator and a `%z` zone offset,
    /// unlike the space-separated, zoneless epics format. Timezone-
    /// independent assertions on structure + distinctness from epics.
    #[test]
    fn string_mode_formats_iso_distinct_from_epics() {
        let secs_since_unix = 1_705_322_096_u64;
        let ts = SystemTime::UNIX_EPOCH + Duration::new(secs_since_unix, 123_456_000);

        let extract = |mode| match TimestampFilter::with_mode(mode)
            .apply(make_event(ts))
            .unwrap()
            .event
            .snapshot
            .value
            .clone()
        {
            EpicsValue::String(s) => s.as_str_lossy().into_owned(),
            other => panic!("expected String, got {other:?}"),
        };
        let iso = extract(TsMode::StringIso);
        let epics = extract(TsMode::StringEpics);

        assert!(iso.contains('T'), "ISO must use a T separator: {iso}");
        assert!(iso.contains(".123456"), "ISO must keep microseconds: {iso}");
        // chrono `%z` appends a 5-char signed offset (`+HHMM` / `-HHMM`).
        let zone = &iso[iso.len() - 5..];
        assert!(
            (zone.starts_with('+') || zone.starts_with('-'))
                && zone[1..].chars().all(|c| c.is_ascii_digit()),
            "ISO must end with a %z zone offset: {iso}"
        );
        assert!(!epics.contains('T'), "epics format must not use T: {epics}");
        assert_ne!(iso, epics, "iso and epics must be distinct strings");
    }
}
