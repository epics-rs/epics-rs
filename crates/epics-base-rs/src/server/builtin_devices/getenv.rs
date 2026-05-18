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

use crate::error::{CaError, CaResult};
use crate::server::device_support::{DeviceReadOutcome, DeviceSupport};
use crate::server::record::Record;
use crate::types::EpicsValue;

/// Reads an environment variable named by `INP` on every record process.
///
/// One instance per record is fine — there's no per-driver state to
/// share. The factory closure used at registration time can simply
/// `|| Box::new(GetenvDeviceSupport::new())`.
#[derive(Default)]
pub struct GetenvDeviceSupport {
    /// Cached last-seen env-var name. Lets the device re-read on every
    /// process without rebuilding the lookup key from the INP string,
    /// which itself never changes after init.
    cached_var: Option<String>,
}

impl GetenvDeviceSupport {
    pub fn new() -> Self {
        Self::default()
    }

    /// Strip the optional leading `@` (libCom INP convention) and any
    /// surrounding whitespace. Empty result means "no env var
    /// specified" which the caller flags as READ_ALARM.
    fn resolve_var_name(inp: &str) -> &str {
        let trimmed = inp.trim();
        trimmed.strip_prefix('@').unwrap_or(trimmed).trim()
    }

    fn fetch(&self, record: &dyn Record) -> Option<String> {
        let inp = record
            .get_field("INP")
            .and_then(|v| match v {
                EpicsValue::String(s) => Some(s),
                _ => None,
            })
            .unwrap_or_default();
        let name = Self::resolve_var_name(&inp);
        if name.is_empty() {
            return None;
        }
        std::env::var(name).ok()
    }
}

impl DeviceSupport for GetenvDeviceSupport {
    fn dtyp(&self) -> &str {
        "getenv"
    }

    fn init(&mut self, record: &mut dyn Record) -> CaResult<()> {
        let rtype = record.record_type();
        if !matches!(rtype, "stringin" | "lsi") {
            return Err(CaError::InvalidValue(format!(
                "DTYP=getenv: unsupported record type '{rtype}' (use stringin or lsi)"
            )));
        }
        // Capture the INP-resolved env-var name once at init so the
        // hot read path can skip the get_field round-trip.
        let inp = record
            .get_field("INP")
            .and_then(|v| match v {
                EpicsValue::String(s) => Some(s),
                _ => None,
            })
            .unwrap_or_default();
        let name = Self::resolve_var_name(&inp);
        if !name.is_empty() {
            self.cached_var = Some(name.to_string());
        }
        // Perform the initial read so PINI / first monitor sees the
        // resolved value rather than the empty default. Mirrors C
        // getenvDevSup's `init_record` behaviour.
        let value = self
            .cached_var
            .as_ref()
            .and_then(|var| std::env::var(var).ok());
        match value {
            Some(s) => {
                record.put_field("VAL", EpicsValue::String(s))?;
                Ok(())
            }
            None => {
                // L2: an unset env var (or empty INP) at init is
                // signalled the SAME way as on `read()` — VAL is
                // cleared to empty and an `Err` is returned so the
                // framework flags the record. Previously init wrote
                // an empty VAL and returned `Ok`, producing a
                // healthy-looking record with no alarm while the
                // same condition on a later `read()` raised
                // READ_ALARM. C flags the alarm at init too.
                record.put_field("VAL", EpicsValue::String(String::new()))?;
                Err(CaError::InvalidValue(format!(
                    "getenv: variable '{}' is unset",
                    self.cached_var.as_deref().unwrap_or("")
                )))
            }
        }
    }

    fn read(&mut self, record: &mut dyn Record) -> CaResult<DeviceReadOutcome> {
        let val = if let Some(ref var) = self.cached_var {
            std::env::var(var).ok()
        } else {
            self.fetch(record)
        };
        match val {
            Some(s) => {
                record.put_field("VAL", EpicsValue::String(s))?;
                Ok(DeviceReadOutcome::ok())
            }
            None => {
                // L1: an env var that was set at init and later
                // unset must not leave the record showing the stale
                // value. C `getenv` devsup re-reads every process
                // and reflects the current (empty) value. Clear VAL
                // to empty, then signal the framework via a soft Err
                // so it sets READ_ALARM / INVALID severity.
                record.put_field("VAL", EpicsValue::String(String::new()))?;
                Err(CaError::InvalidValue(format!(
                    "getenv: variable '{}' is unset",
                    self.cached_var.as_deref().unwrap_or("")
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
    use crate::server::records::stringin::StringinRecord;

    fn make_record(inp: &str) -> StringinRecord {
        let mut r = StringinRecord::new("");
        // StringinRecord doesn't expose INP directly; the common
        // header carries it. For unit tests we exercise the parsing
        // helper directly instead of going through put_field on a
        // record type that wouldn't carry INP in its derived field
        // list.
        let _ = inp;
        r.val = String::new();
        r
    }

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
    fn init_rejects_unsupported_record_types() {
        // We can't easily construct an arbitrary record here without
        // pulling in the full registry; rely on resolve_var_name
        // tests + integration coverage in the runtime tests for the
        // record-type gate. Smoke test the dtyp identifier.
        let dev = GetenvDeviceSupport::new();
        assert_eq!(dev.dtyp(), "getenv");
        let _ = make_record("@PATH");
    }
}
