use std::time::{SystemTime, UNIX_EPOCH};

use epics_base_rs::error::CaResult;
use epics_base_rs::server::device_support::{DeviceReadOutcome, DeviceSupport};
use epics_base_rs::server::record::{ProcessContext, Record};
use epics_base_rs::types::EpicsValue;

use chrono::Local;

/// EPICS epoch offset: seconds from Unix epoch (1970-01-01) to EPICS epoch (1990-01-01).
const EPICS_EPOCH_OFFSET: u64 = 631152000;

/// "Time of Day" device support for stringin records.
///
/// Reads the current time and formats it as a string.
/// Format depends on PHAS field:
/// - PHAS=0: "Mon DD, YYYY HH:MM:SS"
/// - PHAS!=0: "MM/DD/YY HH:MM:SS"
///
/// Ported from `devTimeOfDay.c` (`devSiTodString`).
#[derive(Default)]
pub struct TimeOfDayStringDeviceSupport {
    /// `dbCommon.phas`, captured from the framework's
    /// [`ProcessContext`] before `read()`. C `devTimeOfDay.c:122`
    /// (`createString`) selects the time format from `psi->phas`;
    /// `read()` only gets `&mut dyn Record` and PHAS is a
    /// `CommonFields` field, not a `stringin` record field, so the
    /// framework pushes it through `set_process_context`.
    phas: i16,
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
    }

    fn read(&mut self, record: &mut dyn Record) -> CaResult<DeviceReadOutcome> {
        let now = Local::now();

        // C `devTimeOfDay.c:122` `createString`: `if (psi->phas)`
        // selects the slash format, else the long format. PHAS lives
        // in `CommonFields`; the framework pushed it via
        // `set_process_context`.
        let phas = self.phas;

        let formatted = if phas != 0 {
            now.format("%m/%d/%y %H:%M:%S").to_string()
        } else {
            now.format("%b %d, %Y %H:%M:%S").to_string()
        };

        record.put_field("VAL", EpicsValue::String(formatted))?;
        Ok(DeviceReadOutcome::computed())
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
#[derive(Default)]
pub struct SecPastEpochDeviceSupport {
    /// `dbCommon.phas`, captured from the framework's
    /// [`ProcessContext`] before `read()`. C `devTimeOfDay.c:148`
    /// (`aiReadTs`) adds fractional seconds when `pai->phas` is set.
    phas: i16,
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
    }

    fn read(&mut self, record: &mut dyn Record) -> CaResult<DeviceReadOutcome> {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default();

        let sec_past_epoch = now.as_secs().saturating_sub(EPICS_EPOCH_OFFSET);

        // C `devTimeOfDay.c:148` `aiReadTs`: `if (pai->phas)` adds the
        // nanosecond fraction. PHAS comes from the framework-pushed
        // `ProcessContext`.
        let phas = self.phas;

        let val = if phas != 0 {
            sec_past_epoch as f64 + (now.subsec_nanos() as f64 / 1e9)
        } else {
            sec_past_epoch as f64
        };

        record.put_field("VAL", EpicsValue::Double(val))?;
        Ok(DeviceReadOutcome::computed())
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

    fn ctx_with_phas(phas: i16) -> ProcessContext {
        ProcessContext {
            udf: false,
            udfs: epics_base_rs::server::record::AlarmSeverity::Invalid,
            phas,
            tse: 0,
            tsel: String::new(),
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
            Some(EpicsValue::String(s)) => s,
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
            Some(EpicsValue::String(s)) => s,
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
}
