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
use crate::server::device_support::{
    DeviceAlarm, DeviceInitOutcome, DeviceReadOutcome, DeviceSupport, DeviceUdf,
};
use crate::server::recgbl::alarm_status;
use crate::server::record::{AlarmSeverity, ProcessContext, Record};
use crate::server::records::lsi::LsiRecord;
use crate::types::{EpicsValue, PvString};

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
    /// The env var was unset at the last read, so
    /// [`last_alarm`](DeviceSupport::last_alarm) raises UDF_ALARM at `udfs`.
    unset: bool,
}

impl GetenvDeviceSupport {
    /// Construct from the record's INST_IO `INP`
    /// ([`DeviceSupportContext::inp`](crate::server::ioc_app::DeviceSupportContext)).
    pub fn new(inp: &str) -> Self {
        Self {
            var: Self::resolve_var_name(inp).to_string(),
            udfs: AlarmSeverity::Invalid,
            unset: false,
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

    fn init(&mut self, record: &mut dyn Record) -> CaResult<DeviceInitOutcome> {
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
        Ok(DeviceInitOutcome::ok())
    }

    fn read(&mut self, record: &mut dyn Record) -> CaResult<DeviceReadOutcome> {
        let val = if self.var.is_empty() {
            None
        } else {
            crate::runtime::env::get(&self.var)
        };
        // C `read_lsi` / `read_stringin` (devEnviron.c:52-67, :105-118) write
        // VAL and `prec->udf` and return 0; the unset branch also raises
        // UDF_ALARM at `udfs` with "No such ENV", which `last_alarm()` carries.
        self.unset = val.is_none();
        let udf = if self.unset {
            DeviceUdf::Undefined
        } else {
            DeviceUdf::Defined
        };
        match record
            .as_any_mut()
            .and_then(|a| a.downcast_mut::<LsiRecord>())
        {
            Some(lsi) => lsi.read_environ(val.as_deref().map(std::ffi::OsStr::as_encoded_bytes)),
            None => record.put_field(
                "VAL",
                EpicsValue::String(PvString::from_bytes(
                    val.unwrap_or_default().into_encoded_bytes(),
                )),
            )?,
        }
        Ok(DeviceReadOutcome::converted(udf))
    }

    fn set_process_context(&mut self, ctx: &ProcessContext) {
        // Capture `prec->udfs` so the unset-var alarm is raised at the record's
        // configured UDF severity (C `recGblSetSevrMsg(..., prec->udfs, ...)`),
        // honoring a user-lowered UDFS rather than hardcoding INVALID.
        self.udfs = ctx.udfs;
    }

    fn last_alarm(&self) -> Option<DeviceAlarm<'_>> {
        self.unset.then_some(DeviceAlarm {
            stat: alarm_status::UDF_ALARM,
            sevr: self.udfs as u16,
            msg: "No such ENV",
        })
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

    /// C `read_stringin` copies `getenv`'s bytes into VAL unchecked.
    #[cfg(unix)]
    #[test]
    #[serial_test::serial(epics_env)]
    fn a_non_utf8_value_reaches_val_byte_for_byte() {
        use crate::server::records::stringin::StringinRecord;
        use std::os::unix::ffi::OsStrExt;
        let key = "EPICS_RS_TEST_DEVENVIRON_BYTES";
        // SAFETY: single-threaded under the `epics_env` serial group.
        unsafe { std::env::set_var(key, std::ffi::OsStr::from_bytes(b"a\xffb")) };
        let mut dev = GetenvDeviceSupport::new(&format!("@{key}"));
        let mut rec = StringinRecord::new("");
        dev.read(&mut rec).unwrap();
        // SAFETY: same serial group.
        unsafe { std::env::remove_var(key) };
        assert_eq!(rec.val.as_bytes(), b"a\xffb");
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
