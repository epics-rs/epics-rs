use std::time::{SystemTime, UNIX_EPOCH};

use epics_base_rs::error::CaResult;
use epics_base_rs::server::device_support::{DeviceReadOutcome, DeviceSupport, DeviceUdf};
use epics_base_rs::server::recgbl::get_time_stamp;
use epics_base_rs::server::record::{ProcessContext, Record};
use epics_base_rs::types::EpicsValue;

use chrono::{DateTime, Local};

/// EPICS epoch offset: seconds from Unix epoch (1970-01-01) to EPICS epoch (1990-01-01).
const EPICS_EPOCH_OFFSET: u64 = 631152000;

/// `(secPastEpoch, nsec)` of a resolved time stamp, counted from the EPICS
/// epoch (1990-01-01) exactly as C `epicsTimeStamp`.
///
/// Wrapping, not saturating. C reaches `secPastEpoch` through
/// `epicsTimeFromTime_t` (`$EPICS_BASE/modules/libcom/src/osi/epicsTime.cpp:305-310`
/// at `R7.0.10`), which assigns an `epicsInt64` difference into an
/// `epicsUInt32`, so a clock before 1990 wraps to a 2106 stamp and formats as
/// a date. Saturating pinned it to `0` — C's *uninitialized* value — so a
/// running IOC whose RTC never started answered `<undefined>` where C answers
/// a date, and the `ai` path read `0.0` where C reads ~4.29e9.
fn epics_time_parts(ts: SystemTime) -> (u32, u32) {
    // C's uninitialized `epicsTimeStamp` is the literal `{0, 0}`; this port
    // carries it as `UNIX_EPOCH`, and the `secPastEpoch == 0 && nsec == 0`
    // test below is C's own check for it (`epicsTime.cpp:176`). A sentinel,
    // not a boundary of the conversion.
    if ts == UNIX_EPOCH {
        return (0, 0);
    }
    // `unwrap_or_default` collapses a pre-1970 stamp to the Unix epoch; that
    // is this port's own limit and predates the wrap below.
    let dur = ts.duration_since(UNIX_EPOCH).unwrap_or_default();
    (
        dur.as_secs().wrapping_sub(EPICS_EPOCH_OFFSET) as u32,
        dur.subsec_nanos(),
    )
}

/// "Time of Day" device support for stringin records.
///
/// Reads the current time and formats it as a string.
/// Format depends on PHAS field:
/// - PHAS=0: "Mon DD, YYYY HH:MM:SS"
/// - PHAS!=0: "MM/DD/YY HH:MM:SS"
///
/// Ported from `devTimeOfDay.c` (`devSiTodString`).
pub struct TimeOfDayStringDeviceSupport {
    /// `dbCommon.phas`, captured from the framework's
    /// [`ProcessContext`] before `read()`. C `devTimeOfDay.c:122`
    /// (`createString`) selects the time format from `psi->phas`;
    /// `read()` only gets `&mut dyn Record` and PHAS is a
    /// `CommonFields` field, not a `stringin` record field, so the
    /// framework pushes it through `set_process_context`.
    phas: i16,
    /// `dbCommon.tse` and the record's current `dbCommon.time`, captured
    /// from [`ProcessContext`]. C `createString` calls
    /// `recGblGetTimeStamp(psi)` to resolve `psi->time` from `psi->tse`
    /// *before* formatting, then formats that resolved stamp — not the
    /// wall clock. `read()` resolves the same way via
    /// [`get_time_stamp`]`(tse, time)`.
    tse: i16,
    time: SystemTime,
}

impl Default for TimeOfDayStringDeviceSupport {
    fn default() -> Self {
        Self {
            phas: 0,
            tse: 0,
            time: SystemTime::UNIX_EPOCH,
        }
    }
}

impl TimeOfDayStringDeviceSupport {
    pub fn new() -> Self {
        Self::default()
    }
}

impl DeviceSupport for TimeOfDayStringDeviceSupport {
    fn dtyp(&self) -> &str {
        "Time of Day"
    }

    fn set_process_context(&mut self, ctx: &ProcessContext) {
        self.phas = ctx.phas;
        self.tse = ctx.tse;
        self.time = ctx.time;
    }

    fn read(&mut self, record: &mut dyn Record) -> CaResult<DeviceReadOutcome> {
        // C `devTimeOfDay.c:121` `createString`: `recGblGetTimeStamp(psi)`
        // resolves `psi->time` from TSE *now*, then formats that stamp.
        // C `recGblGetTimeStamp` leaves `prec->time` alone when
        // `epicsTimeGetEvent` fails, and the record-level owner
        // (`apply_timestamp`) emits the errlog this cycle.
        let ts = get_time_stamp(self.tse, self.time).unwrap_or(self.time);
        let (sec_past_epoch, nsec) = epics_time_parts(ts);

        // C `devTimeOfDay.c:122` `createString`: `if (psi->phas)` selects
        // the slash format, else the long format. PHAS lives in
        // `CommonFields`; the framework pushed it via `set_process_context`.
        let phas = self.phas;

        let formatted = if sec_past_epoch == 0 && nsec == 0 {
            // C `epicsTimeToStrftime` (epicsTime.cpp:176-180): an epoch
            // (uninitialized) stamp formats to the literal "<undefined>".
            // `createString` then truncates at the last '.', which leaves
            // this string unchanged (it has none).
            "<undefined>".to_string()
        } else {
            // C formats with a trailing `.%09f` then truncates at the last
            // '.', so the result is whole-second resolution either way.
            let local: DateTime<Local> = DateTime::<Local>::from(ts);
            if phas != 0 {
                local.format("%m/%d/%y %H:%M:%S").to_string()
            } else {
                local.format("%b %d, %Y %H:%M:%S").to_string()
            }
        };

        record.put_field("VAL", EpicsValue::String(formatted.into()))?;
        // C `devTimeOfDay.c::stringinReadTs:135-137` — `psi->udf = 0;
        // return(0)`. stringin has no RVAL, so 0 is a plain success.
        Ok(DeviceReadOutcome::converted(DeviceUdf::Defined))
    }

    fn write(&mut self, _record: &mut dyn Record) -> CaResult<()> {
        Ok(())
    }
}

/// "Sec Past Epoch" device support for ai records.
///
/// Reads the current time as seconds past the EPICS epoch (1990-01-01).
/// If PHAS field is nonzero, includes fractional seconds.
///
/// Ported from `devTimeOfDay.c` (`devAiTodSeconds`).
pub struct SecPastEpochDeviceSupport {
    /// `dbCommon.phas`, captured from the framework's
    /// [`ProcessContext`] before `read()`. C `devTimeOfDay.c:148`
    /// (`aiReadTs`) adds fractional seconds when `pai->phas` is set.
    phas: i16,
    /// `dbCommon.tse` and the record's current `dbCommon.time`. C
    /// `aiReadTs` calls `recGblGetTimeStamp(pai)` to resolve `pai->time`
    /// from `pai->tse`, then reads `pai->time.secPastEpoch` — not the
    /// wall clock. `read()` resolves the same way via
    /// [`get_time_stamp`]`(tse, time)`.
    tse: i16,
    time: SystemTime,
}

impl Default for SecPastEpochDeviceSupport {
    fn default() -> Self {
        Self {
            phas: 0,
            tse: 0,
            time: SystemTime::UNIX_EPOCH,
        }
    }
}

impl SecPastEpochDeviceSupport {
    pub fn new() -> Self {
        Self::default()
    }
}

impl DeviceSupport for SecPastEpochDeviceSupport {
    fn dtyp(&self) -> &str {
        "Sec Past Epoch"
    }

    fn set_process_context(&mut self, ctx: &ProcessContext) {
        self.phas = ctx.phas;
        self.tse = ctx.tse;
        self.time = ctx.time;
    }

    fn read(&mut self, record: &mut dyn Record) -> CaResult<DeviceReadOutcome> {
        // C `devTimeOfDay.c:145` `aiReadTs`: `recGblGetTimeStamp(pai)`
        // resolves `pai->time` from TSE, then `pai->val =
        // pai->time.secPastEpoch`. An epoch (uninitialized) stamp yields
        // `secPastEpoch == 0`, so `val == 0.0` — no separate sentinel is
        // needed for the ai path.
        // C `recGblGetTimeStamp` leaves `prec->time` alone when
        // `epicsTimeGetEvent` fails, and the record-level owner
        // (`apply_timestamp`) emits the errlog this cycle.
        let ts = get_time_stamp(self.tse, self.time).unwrap_or(self.time);
        let (sec_past_epoch, nsec) = epics_time_parts(ts);

        // C `devTimeOfDay.c:148` `aiReadTs`: `if (pai->phas)` adds the
        // nanosecond fraction. PHAS comes from the framework-pushed
        // `ProcessContext`.
        let phas = self.phas;

        let val = if phas != 0 {
            sec_past_epoch as f64 + (nsec as f64 / 1e9)
        } else {
            sec_past_epoch as f64
        };

        record.put_field("VAL", EpicsValue::Double(val))?;
        // C `devTimeOfDay.c::aiReadTs:150-152` — `pai->udf = 0; return(2)`:
        // VAL written directly and the record declared defined.
        Ok(DeviceReadOutcome::computed(DeviceUdf::Defined))
    }

    fn write(&mut self, _record: &mut dyn Record) -> CaResult<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use epics_base_rs::server::record::ProcessContext;
    use epics_base_rs::server::records::stringin::StringinRecord;
    use std::time::Duration;

    fn ctx_with_phas(phas: i16) -> ProcessContext {
        ProcessContext {
            udf: false,
            udfs: epics_base_rs::server::record::AlarmSeverity::Invalid,
            nsev: epics_base_rs::server::record::AlarmSeverity::NoAlarm,
            phas,
            tse: 0,
            time: SystemTime::UNIX_EPOCH,
            tsel: String::new(),
            dtyp: String::new(),
            callback_priority: epics_base_rs::runtime::task::CallbackPriority::Low,
        }
    }

    /// A `ProcessContext` carrying an explicit device-time stamp with
    /// `tse = -2` (`epicsTimeEventDeviceTime`), so `read()` resolves the
    /// stamp to exactly `time` via `get_time_stamp(-2, time)`.
    fn ctx_device_time(phas: i16, time: SystemTime) -> ProcessContext {
        ProcessContext {
            udf: false,
            udfs: epics_base_rs::server::record::AlarmSeverity::Invalid,
            nsev: epics_base_rs::server::record::AlarmSeverity::NoAlarm,
            phas,
            tse: -2,
            time,
            tsel: String::new(),
            dtyp: String::new(),
            callback_priority: epics_base_rs::runtime::task::CallbackPriority::Low,
        }
    }

    /// C `devTimeOfDay.c:122-127` `createString`: `if (psi->phas)`
    /// picks the `MM/DD/YY HH:MM:SS` slash format, else
    /// `Mon DD, YYYY HH:MM:SS`. PHAS is a `dbCommon` field, not a
    /// `stringin` field, so the framework pushes it via
    /// `set_process_context` — `record.get_field("PHAS")` returns None.
    #[test]
    fn time_of_day_phas_zero_uses_long_format() {
        let mut dev = TimeOfDayStringDeviceSupport::new();
        let mut rec = StringinRecord::new("");
        dev.set_process_context(&ctx_with_phas(0));
        dev.read(&mut rec).unwrap();
        let val = match rec.get_field("VAL") {
            Some(EpicsValue::String(s)) => s.as_str_lossy().into_owned(),
            other => panic!("expected String VAL, got {other:?}"),
        };
        // "%b %d, %Y %H:%M:%S" — contains a comma, no slashes.
        assert!(val.contains(','), "PHAS=0 long format has a comma: {val}");
        assert!(
            !val.contains('/'),
            "PHAS=0 long format has no slashes: {val}"
        );
    }

    /// C `devTimeOfDay.c:123`: PHAS != 0 selects `%m/%d/%y %H:%M:%S`.
    /// Before the framework `set_process_context` wiring this branch was
    /// never reached — `get_field("PHAS")` always returned None.
    #[test]
    fn time_of_day_phas_nonzero_uses_slash_format() {
        let mut dev = TimeOfDayStringDeviceSupport::new();
        let mut rec = StringinRecord::new("");
        dev.set_process_context(&ctx_with_phas(1));
        dev.read(&mut rec).unwrap();
        let val = match rec.get_field("VAL") {
            Some(EpicsValue::String(s)) => s.as_str_lossy().into_owned(),
            other => panic!("expected String VAL, got {other:?}"),
        };
        // "%m/%d/%y %H:%M:%S" — two slashes, no comma.
        assert_eq!(
            val.matches('/').count(),
            2,
            "PHAS!=0 slash format has two slashes: {val}"
        );
        assert!(
            !val.contains(','),
            "PHAS!=0 slash format has no comma: {val}"
        );
    }

    /// C `devTimeOfDay.c:148` `aiReadTs`: `if (pai->phas)` adds the
    /// nanosecond fraction to the seconds count. PHAS=0 yields a whole
    /// number; PHAS!=0 generally yields a fraction.
    #[test]
    fn sec_past_epoch_phas_zero_is_whole_seconds() {
        let mut dev = SecPastEpochDeviceSupport::new();
        let mut rec = epics_base_rs::server::records::ai::AiRecord::new(0.0);
        dev.set_process_context(&ctx_with_phas(0));
        dev.read(&mut rec).unwrap();
        let val = match rec.get_field("VAL") {
            Some(EpicsValue::Double(v)) => v,
            other => panic!("expected Double VAL, got {other:?}"),
        };
        assert_eq!(
            val.fract(),
            0.0,
            "PHAS=0 must yield whole seconds, got {val}"
        );
    }

    /// C `epicsTimeToStrftime` (epicsTime.cpp:176-180): a stamp at the
    /// Unix epoch (`secPastEpoch == 0`) is "uninitialized" and formats to
    /// the literal "<undefined>". `read()` resolves `tse = -2` to the
    /// supplied device time verbatim, so an epoch device time must reach
    /// that branch instead of the wall clock.
    #[test]
    fn time_of_day_epoch_zero_is_undefined() {
        let mut dev = TimeOfDayStringDeviceSupport::new();
        let mut rec = StringinRecord::new("");
        dev.set_process_context(&ctx_device_time(0, SystemTime::UNIX_EPOCH));
        dev.read(&mut rec).unwrap();
        let val = match rec.get_field("VAL") {
            Some(EpicsValue::String(s)) => s.as_str_lossy().into_owned(),
            other => panic!("expected String VAL, got {other:?}"),
        };
        assert_eq!(val, "<undefined>", "epoch stamp must format as sentinel");
    }

    /// C counts `secPastEpoch` from the EPICS epoch (1990-01-01), so a
    /// stamp *at* the EPICS epoch also has `secPastEpoch == 0` and formats
    /// to "<undefined>" (`epicsTime.cpp:176`).
    #[test]
    fn time_of_day_epics_epoch_is_undefined() {
        let mut dev = TimeOfDayStringDeviceSupport::new();
        let mut rec = StringinRecord::new("");
        let epics_epoch = SystemTime::UNIX_EPOCH + Duration::from_secs(EPICS_EPOCH_OFFSET);
        dev.set_process_context(&ctx_device_time(0, epics_epoch));
        dev.read(&mut rec).unwrap();
        let val = match rec.get_field("VAL") {
            Some(EpicsValue::String(s)) => s.as_str_lossy().into_owned(),
            other => panic!("expected String VAL, got {other:?}"),
        };
        assert_eq!(val, "<undefined>", "EPICS-epoch stamp is also undefined");
    }

    /// One second BEFORE the EPICS epoch is a running clock, not an
    /// uninitialized stamp. C's `epicsTimeFromTime_t` wraps it into
    /// `epicsUInt32` (`epicsTime.cpp:305-310`), giving 0xFFFF_FFFF, which
    /// `epicsTimeToStrftime` formats as a 2106 date. Saturating answered 0
    /// and printed the "<undefined>" sentinel instead.
    #[test]
    fn time_of_day_pre_1990_clock_is_a_date_not_the_undefined_sentinel() {
        let mut dev = TimeOfDayStringDeviceSupport::new();
        let mut rec = StringinRecord::new("");
        let before = SystemTime::UNIX_EPOCH + Duration::from_secs(EPICS_EPOCH_OFFSET - 1);
        dev.set_process_context(&ctx_device_time(0, before));
        dev.read(&mut rec).unwrap();
        let val = match rec.get_field("VAL") {
            Some(EpicsValue::String(s)) => s.as_str_lossy().into_owned(),
            other => panic!("expected String VAL, got {other:?}"),
        };
        assert_ne!(val, "<undefined>", "a pre-1990 clock is not C's {{0, 0}}");
    }

    /// The wrap itself, read straight off the conversion.
    #[test]
    fn epics_time_parts_wraps_both_ends_and_keeps_the_unset_sentinel() {
        assert_eq!(epics_time_parts(SystemTime::UNIX_EPOCH), (0, 0));
        let at = SystemTime::UNIX_EPOCH + Duration::from_secs(EPICS_EPOCH_OFFSET);
        assert_eq!(epics_time_parts(at).0, 0);
        let before = SystemTime::UNIX_EPOCH + Duration::from_secs(EPICS_EPOCH_OFFSET - 1);
        assert_eq!(epics_time_parts(before).0, u32::MAX);
        let last =
            SystemTime::UNIX_EPOCH + Duration::from_secs(EPICS_EPOCH_OFFSET + u32::MAX as u64);
        assert_eq!(epics_time_parts(last).0, u32::MAX);
        let past =
            SystemTime::UNIX_EPOCH + Duration::from_secs(EPICS_EPOCH_OFFSET + u32::MAX as u64 + 1);
        assert_eq!(epics_time_parts(past).0, 0);
    }

    /// C `createString` formats `psi->time` (resolved from TSE), NOT the
    /// wall clock. A `tse = -2` device time of Unix 1_000_000_000
    /// (2001-09-09 UTC) must format to its own year, never the current
    /// year, and is not the undefined sentinel.
    #[test]
    fn time_of_day_resolves_device_time_not_wall_clock() {
        let mut dev = TimeOfDayStringDeviceSupport::new();
        let mut rec = StringinRecord::new("");
        let device_time = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000_000);
        dev.set_process_context(&ctx_device_time(0, device_time));
        dev.read(&mut rec).unwrap();
        let val = match rec.get_field("VAL") {
            Some(EpicsValue::String(s)) => s.as_str_lossy().into_owned(),
            other => panic!("expected String VAL, got {other:?}"),
        };
        // Unix 1e9 is 2001-09-09 01:46:40 UTC — every real time zone keeps
        // the year 2001. Proves the device time, not 2026 wall clock, was
        // formatted.
        assert!(val.contains("2001"), "must format the device time: {val}");
        assert_ne!(val, "<undefined>");
    }

    /// C `aiReadTs` reads `pai->time.secPastEpoch` from the TSE-resolved
    /// stamp. An epoch device time yields `secPastEpoch == 0`, so
    /// `val == 0.0` — no sentinel needed.
    #[test]
    fn sec_past_epoch_epoch_zero_is_zero() {
        let mut dev = SecPastEpochDeviceSupport::new();
        let mut rec = epics_base_rs::server::records::ai::AiRecord::new(0.0);
        dev.set_process_context(&ctx_device_time(0, SystemTime::UNIX_EPOCH));
        dev.read(&mut rec).unwrap();
        let val = match rec.get_field("VAL") {
            Some(EpicsValue::Double(v)) => v,
            other => panic!("expected Double VAL, got {other:?}"),
        };
        assert_eq!(val, 0.0, "epoch stamp yields secPastEpoch 0");
    }

    /// C `aiReadTs`: `pai->val = pai->time.secPastEpoch` (EPICS epoch
    /// based). For Unix 1_000_000_000 that is `1_000_000_000 -
    /// EPICS_EPOCH_OFFSET`. With PHAS != 0 the nanosecond fraction is
    /// added (`devTimeOfDay.c:148`).
    #[test]
    fn sec_past_epoch_resolves_device_time_with_fraction() {
        let device_time = SystemTime::UNIX_EPOCH
            + Duration::from_secs(1_000_000_000)
            + Duration::from_nanos(500_000_000);
        let expected_secs = (1_000_000_000u64 - EPICS_EPOCH_OFFSET) as f64;

        // PHAS=0: whole seconds, fraction dropped.
        let mut dev = SecPastEpochDeviceSupport::new();
        let mut rec = epics_base_rs::server::records::ai::AiRecord::new(0.0);
        dev.set_process_context(&ctx_device_time(0, device_time));
        dev.read(&mut rec).unwrap();
        match rec.get_field("VAL") {
            Some(EpicsValue::Double(v)) => assert_eq!(v, expected_secs),
            other => panic!("expected Double VAL, got {other:?}"),
        };

        // PHAS!=0: adds the 0.5 s fraction.
        let mut dev = SecPastEpochDeviceSupport::new();
        let mut rec = epics_base_rs::server::records::ai::AiRecord::new(0.0);
        dev.set_process_context(&ctx_device_time(1, device_time));
        dev.read(&mut rec).unwrap();
        match rec.get_field("VAL") {
            Some(EpicsValue::Double(v)) => assert_eq!(v, expected_secs + 0.5),
            other => panic!("expected Double VAL, got {other:?}"),
        };
    }
}
