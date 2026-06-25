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
//! READ_ALARM. Supported record types: `stringin`, `lsi`. Anything
//! else returns an [`UnsupportedRecord`](epics-base style) error and
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
use crate::server::record::Record;
use crate::types::EpicsValue;

/// Reads the environment variable named by the INST_IO `INP` on every record
/// process. The variable name is resolved once from `ctx.inp` at construction.
pub struct GetenvDeviceSupport {
    /// Env-var name resolved from `INP` (post-`@`, trimmed); empty when the
    /// INP payload was empty (treated as "unknown env var").
    var: String,
}

impl GetenvDeviceSupport {
    /// Construct from the record's INST_IO `INP`
    /// ([`DeviceSupportContext::inp`](crate::server::ioc_app::DeviceSupportContext)).
    pub fn new(inp: &str) -> Self {
        Self {
            var: Self::resolve_var_name(inp).to_string(),
        }
    }

    /// Strip the optional leading `@` (libCom INP convention) and any
    /// surrounding whitespace. Empty result means "no env var
    /// specified" which the caller flags as READ_ALARM.
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
                record.put_field("VAL", EpicsValue::String(s.into()))?;
                Ok(DeviceReadOutcome::ok())
            }
            None => {
                // Unset env var. C clears VAL and raises UDF, returning SUCCESS:
                //   read_lsi      (devEnviron.c:62-67):  val[0]=0; len=1;
                //     udf=TRUE; recGblSetSevrMsg(prec, UDF_ALARM, prec->udfs,
                //     "No such ENV");  return 0;
                //   read_stringin (devEnviron.c:114-118): same, without len.
                //
                // base-rs cannot reproduce that exactly, and the gap is the same
                // `value_is_undefined`-derived udf model already disclosed for
                // Db State (dbstate.rs): a device sees only the inner record (no
                // `dbCommon`, and no alarm/udf-setting `ProcessAction`), so it
                // cannot force `udf=TRUE` on a *defined* value. The framework
                // recomputes `udf = value_is_undefined()` after process, and for
                // a string ANY value — including the empty string C writes — is
                // defined (record_trait.rs `value_is_undefined`: `Some(_) =>
                // false`), so clearing VAL to "" would clear udf and raise NO
                // alarm at all. To still flag the record we clear the stale value
                // (a var that was set then later unset must not keep showing the
                // old value) and return a soft Err, which the framework turns
                // into READ_ALARM at INVALID (processing.rs). Accepted DIVERGENCE
                // vs C (disclosed, not faked):
                //   - alarm STAT:  READ_ALARM vs C UDF_ALARM (wire-observable);
                //   - severity:    INVALID vs C `prec->udfs` (coincide on the
                //     default UDFS=INVALID; diverge only if the user lowers it);
                //   - udf flag:    stays FALSE (defined empty VAL) vs C udf=TRUE;
                //   - AMSG "No such ENV" + lsi LEN=1: not modeled.
                // Closing this fully needs a framework device-settable udf/alarm
                // path (the same root as the Db State divergence), out of scope
                // for getenv alone.
                record.put_field("VAL", EpicsValue::String(String::new().into()))?;
                Err(CaError::InvalidValue(format!(
                    "getenv: variable '{}' is unset",
                    self.var
                )))
            }
        }
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
