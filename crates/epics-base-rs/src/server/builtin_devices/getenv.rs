//! `getenv` device support — read an environment variable on every
//! process and store the value into the record's `VAL` field.
//!
//! Mirrors epics-base 3.15.4 (`getenvDevSup`): the canonical use case
//! is exposing host metadata (`HOSTNAME`, `IOC_NAME`, `EPICS_BASE`)
//! through a CA-readable PV without writing custom device support.
//!
//! Wiring (in a `.db` file):
//!
//! ```text
//! record(stringin, "$(P)hostname") {
//!     field(DTYP, "getenv")
//!     field(INP,  "@HOSTNAME")
//! }
//! ```
//!
//! The INP payload is the env-var name, with an optional leading `@`
//! (carried over from C macro syntax) silently stripped. Empty payload
//! is treated as "unknown env var" and produces an empty VAL with a
//! UDF_ALARM. Supported record types: `stringin`, `lsi`. Anything
//! else returns an `UnsupportedRecord` (epics-base style) error and
//! flags the record alarm.
//!
//! The env-var name comes from the record's INST_IO `INP`, which only the
//! runtime [`DeviceSupportContext`](crate::server::ioc_app::DeviceSupportContext)
//! carries (the inner record handed to `init`/`read` does NOT expose `INP` —
//! `INP` lives on the `RecordInstance` common header, not the typed record).
//! So getenv is dispatched by the dynamic factory alongside the other
//! INP/OUT-needing builtins (`Soft Timestamp`, `stdio`, `Db State`), receiving
//! `ctx.inp` at construction — not registered as a context-free static factory.

use crate::error::{CaError, CaResult};
use crate::server::device_support::{DeviceReadOutcome, DeviceSupport};
use crate::server::recgbl::alarm_status;
use crate::server::record::{AlarmSeverity, ProcessContext, Record};
use crate::types::EpicsValue;

/// Reads the environment variable named by the INST_IO `INP` on every record
/// process. The variable name is resolved once from `ctx.inp` at construction.
pub struct GetenvDeviceSupport {
    /// Env-var name resolved from `INP` (post-`@`, trimmed); empty when the
    /// INP payload was empty (treated as "unknown env var").
    var: String,
    /// `prec->udfs` — the severity C raises an unset env var at
    /// (`recGblSetSevrMsg(prec, UDF_ALARM, prec->udfs, …)`). Captured from the
    /// process context before each read so a user-set UDFS is honored; defaults
    /// to INVALID (the dbCommon UDFS default) until the first context push.
    udfs: AlarmSeverity,
    /// Alarm surfaced via [`last_alarm`](DeviceSupport::last_alarm) this cycle:
    /// `Some((UDF_ALARM, udfs))` while the env var is unset, `None` once it
    /// resolves. The framework applies it via `rec_gbl_set_sevr`.
    unset_alarm: Option<(u16, u16)>,
}

impl GetenvDeviceSupport {
    /// Construct from the record's INST_IO `INP`
    /// ([`DeviceSupportContext::inp`](crate::server::ioc_app::DeviceSupportContext)).
    pub fn new(inp: &str) -> Self {
        Self {
            var: Self::resolve_var_name(inp).to_string(),
            udfs: AlarmSeverity::Invalid,
            unset_alarm: None,
        }
    }

    /// Strip the optional leading `@` (libCom INP convention) and any
    /// surrounding whitespace. Empty result means "no env var
    /// specified" which the caller flags as UDF_ALARM.
    fn resolve_var_name(inp: &str) -> &str {
        let trimmed = inp.trim();
        trimmed.strip_prefix('@').unwrap_or(trimmed).trim()
    }
}

impl DeviceSupport for GetenvDeviceSupport {
    fn dtyp(&self) -> &str {
        "getenv"
    }

    fn init(&mut self, record: &mut dyn Record) -> CaResult<()> {
        // C's getenv dset has a NULL `init_record` slot ({5, NULL, init_lsi,
        // NULL, NULL}); per-record setup is the dsxt `add_lsi` / `add_stringin`
        // (devEnviron.c), which only validate `inp.type == INST_IO` and write
        // neither VAL nor an alarm. The env var is read lazily at record
        // process (`read_lsi` / `read_stringin`), so init only gates the record
        // type here — matching C and the `Soft Timestamp` sibling. VAL is
        // populated on the record's first process (PINI / scan), not at build.
        let rtype = record.record_type();
        if !matches!(rtype, "stringin" | "lsi") {
            return Err(CaError::InvalidValue(format!(
                "DTYP=getenv: unsupported record type '{rtype}' (use stringin or lsi)"
            )));
        }
        Ok(())
    }

    fn read(&mut self, record: &mut dyn Record) -> CaResult<DeviceReadOutcome> {
        let val = if self.var.is_empty() {
            None
        } else {
            std::env::var(&self.var).ok()
        };
        match val {
            Some(s) => {
                // C set path (read_lsi devEnviron.c:56-61 / read_stringin
                // :109-112): copy value into VAL, udf=FALSE. Clear our pending
                // unset alarm so `last_alarm()` stops raising it once the var
                // reappears.
                self.unset_alarm = None;
                record.put_field("VAL", EpicsValue::String(s.into()))?;
                Ok(DeviceReadOutcome::ok())
            }
            None => {
                // C unset path (read_lsi devEnviron.c:62-67 / read_stringin
                // :114-118): `val[0]=0; udf=TRUE; recGblSetSevrMsg(prec,
                // UDF_ALARM, prec->udfs, "No such ENV"); return 0` (read_lsi also
                // sets len=1). Clear the stale value (a var set then later unset
                // must not keep showing the old value) and surface UDF_ALARM at
                // `udfs` through `last_alarm()` — the device→framework alarm
                // channel the processing loop applies via `rec_gbl_set_sevr`
                // (the same recGblSetSevr merge C uses, processing.rs) — instead
                // of returning Err, which would raise READ_ALARM (wrong STAT) and
                // log to stderr every cycle (C is silent). This matches C's STAT
                // (UDF_ALARM) and severity (`prec->udfs`, captured in
                // set_process_context).
                //
                // Residual divergences vs C (narrow, deferred — not the alarm):
                //   - the `udf` field itself reads 0, not C's TRUE: base-rs
                //     derives `udf` from `value_is_undefined()`, and a string —
                //     including the empty string written here — is defined
                //     (record_trait.rs `Some(_) => false`); the *alarm* is raised
                //     directly, but the raw UDF field has no device→udf channel.
                //   - AMSG "No such ENV" is not carried (`last_alarm()` is
                //     (status, severity) only), and lsi LEN is not modeled (C
                //     sets it to strlen(val) on the set path, 1 on unset).
                self.unset_alarm = Some((alarm_status::UDF_ALARM, self.udfs as u16));
                record.put_field("VAL", EpicsValue::String(String::new().into()))?;
                Ok(DeviceReadOutcome::ok())
            }
        }
    }

    fn set_process_context(&mut self, ctx: &ProcessContext) {
        // Capture `prec->udfs` so the unset-var alarm is raised at the record's
        // configured UDF severity (C `recGblSetSevrMsg(..., prec->udfs, ...)`),
        // honoring a user-lowered UDFS rather than hardcoding INVALID.
        self.udfs = ctx.udfs;
    }

    fn last_alarm(&self) -> Option<(u16, u16)> {
        self.unset_alarm
    }

    fn write(&mut self, _record: &mut dyn Record) -> CaResult<()> {
        Err(CaError::InvalidValue(
            "getenv device support is read-only (stringin / lsi only)".into(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::records::ai::AiRecord;

    #[test]
    fn resolve_var_name_strips_at_prefix() {
        assert_eq!(
            GetenvDeviceSupport::resolve_var_name("@HOSTNAME"),
            "HOSTNAME"
        );
        assert_eq!(
            GetenvDeviceSupport::resolve_var_name("  HOSTNAME  "),
            "HOSTNAME"
        );
        assert_eq!(
            GetenvDeviceSupport::resolve_var_name("@ HOSTNAME"),
            "HOSTNAME"
        );
        assert_eq!(GetenvDeviceSupport::resolve_var_name(""), "");
        assert_eq!(GetenvDeviceSupport::resolve_var_name("@"), "");
    }

    #[test]
    fn new_resolves_var_from_inp() {
        assert_eq!(GetenvDeviceSupport::new("@HOSTNAME").var, "HOSTNAME");
        assert_eq!(GetenvDeviceSupport::new("  PATH  ").var, "PATH");
        assert!(GetenvDeviceSupport::new("@").var.is_empty());
        assert_eq!(GetenvDeviceSupport::new("@PATH").dtyp(), "getenv");
    }

    #[test]
    fn init_rejects_unsupported_record_type() {
        // ai is not a supported getenv record type → init Errs (the same gate
        // wire_device_to_record turns into an INVALID record at build time).
        let mut dev = GetenvDeviceSupport::new("@PATH");
        let mut rec = AiRecord::new(0.0);
        assert!(dev.init(&mut rec).is_err());
    }
}
