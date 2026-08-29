//! `Db State` device support — EPICS base `devBiDbState.c` / `devBoDbState.c`
//! (`devSoft.dbd`).
//!
//! One DTYP (`"Db State"`, INST_IO) shared by two records — `bi` (input,
//! `devBiDbState`) and `bo` (output, `devBoDbState`). The INST_IO `INP` (bi) /
//! `OUT` (bo) instio string names a process-global named boolean in the
//! `dbState` registry; `bi` reads the bit into `VAL`, `bo` sets/clears it from
//! `VAL`.
//!
//! C `read_bi` (`devBiDbState.c:63-71`): `if (dpvt) { prec->val =
//! dbStateGet(dpvt); prec->udf = FALSE; } return 2` — VAL written directly, so
//! the bi conversion is skipped (return 2). C `write_bo` (`devBoDbState.c:62-69`):
//! `if (prec->val) dbStateSet(dpvt) else dbStateClear(dpvt)`. The state handle
//! is resolved once by `add_record` from the instio string via `dbStateFind`,
//! creating it (with a one-time `errlogInfo` notice) when absent; an empty
//! instio string leaves `dpvt = NULL`, so read/write no-op.
//!
//! The same `dbState` registry backs the `sync` JSON filter (`database::
//! filters::sync`), so a `Db State` bo and a `sync`-filtered monitor observe
//! the same named bit.

use std::sync::Arc;

use crate::error::{CaError, CaResult};
use crate::runtime::log::{ErrlogSevEnum, errlog_sev_printf};
use crate::server::database::filters::sync::{DbState, db_state_registry};
use crate::server::device_support::{
    DeviceInitOutcome, DeviceReadOutcome, DeviceSupport, DeviceUdf,
};
use crate::server::record::Record;
use crate::types::EpicsValue;

/// `Db State` device support for the `bi` / `bo` records.
pub struct DbStateDeviceSupport {
    /// INST_IO `INP` string — the `bi` state-name source.
    inp: String,
    /// INST_IO `OUT` string — the `bo` state-name source.
    out: String,
    /// Resolved named state; `None` when the instio name was empty
    /// (C `dpvt == NULL` → read/write no-op).
    state: Option<Arc<DbState>>,
}

impl DbStateDeviceSupport {
    /// Construct from the record's INST_IO `INP` and `OUT`
    /// ([`DeviceSupportContext`](crate::server::ioc_app::DeviceSupportContext)).
    /// The state name is resolved in [`init`](DbStateDeviceSupport::init),
    /// where the record type selects `INP` (bi) or `OUT` (bo).
    pub fn new(inp: &str, out: &str) -> Self {
        Self {
            inp: inp.to_string(),
            out: out.to_string(),
            state: None,
        }
    }
}

impl DeviceSupport for DbStateDeviceSupport {
    fn dtyp(&self) -> &str {
        "Db State"
    }

    fn init(&mut self, record: &mut dyn Record) -> CaResult<DeviceInitOutcome> {
        // C registers a SEPARATE dset per record type (devBiDbState for bi,
        // devBoDbState for bo); both carry DTYP "Db State". The bi dset reads
        // the state name from INP, the bo dset from OUT. The Rust dynamic
        // factory builds one device for the DTYP regardless of record type, so
        // gate the record type here and pick the matching instio source.
        let rt = record.record_type();
        let raw = match rt {
            "bi" => self.inp.as_str(),
            "bo" => self.out.as_str(),
            other => {
                return Err(CaError::InvalidValue(format!(
                    "DTYP='Db State': unsupported record type '{other}' (use bi or bo)"
                )));
            }
        };

        // C `add_record` reads `inp/out.value.instio.string` (the post-`@`
        // content). An empty string leaves `dpvt = NULL` and the record never
        // resolves a state — read/write no-op.
        let name = raw.strip_prefix('@').unwrap_or(raw);
        if name.is_empty() {
            self.state = None;
            return Ok(DeviceInitOutcome::Live);
        }

        // C `add_record`: `dbStateFind`, and if absent AND non-empty, log an
        // `errlogInfo` notice then `dbStateCreate`. The notice reaches the
        // console: `errlogSevVprintf` (`errlog.c:379-391`) consults no
        // threshold, so `sevToLog` cannot hide it whatever it is set to.
        let registry = db_state_registry();
        self.state = Some(match registry.find(name) {
            Some(s) => s,
            None => {
                let devsup = if rt == "bi" {
                    "devBiDbState"
                } else {
                    "devBoDbState"
                };
                errlog_sev_printf(
                    ErrlogSevEnum::Info,
                    &format!("{devsup}: Creating new db state '{name}'\n"),
                );
                registry.get_or_create(name)
            }
        });
        Ok(DeviceInitOutcome::Live)
    }

    fn read(&mut self, record: &mut dyn Record) -> CaResult<DeviceReadOutcome> {
        // C `read_bi` (devBiDbState.c:63-71): `if (dpvt) { prec->val =
        // dbStateGet(dpvt); prec->udf = FALSE; } return 2`. Both halves are
        // stated here — VAL written directly (`computed`, C return 2) and the
        // record thereby defined (`DeviceUdf::Defined`, C's `prec->udf =
        // FALSE`) — because bi's `process()` re-derives neither: the fold sits
        // after the UDF assignment (biRecord.c:136-141), so the dset's write
        // is the only one there is.
        //
        // With no state (empty instio name) C's `if (dpvt)` guard skips BOTH the
        // VAL write and the `udf = FALSE`, so an unconfigured bi keeps
        // `udf = TRUE` and raises UDF_ALARM/INVALID on every cycle. That arm
        // reports the read for what it is: nothing was produced, VAL and UDF
        // both stand. C's literal `return 2` and a `-1` are indistinguishable
        // to a bi for the same reason.
        let Some(state) = &self.state else {
            return Ok(DeviceReadOutcome::failed());
        };
        let bit = u16::from(state.get());
        record.put_field("VAL", EpicsValue::Enum(bit))?;
        Ok(DeviceReadOutcome::computed(DeviceUdf::Defined))
    }

    fn write(&mut self, record: &mut dyn Record) -> CaResult<()> {
        // C `write_bo` (devBoDbState.c:62-69): `if (prec->val)
        // dbStateSet(dpvt) else dbStateClear(dpvt)`. C tests the raw integer
        // VAL for nonzero; bo VAL is an `Enum` 0/1, but read it as any integer
        // so the nonzero test matches C regardless of the carried variant.
        if let Some(state) = &self.state {
            let on = match record.get_field("VAL") {
                Some(EpicsValue::Enum(v)) | Some(EpicsValue::UShort(v)) => v != 0,
                Some(EpicsValue::Short(v)) => v != 0,
                Some(EpicsValue::Long(v)) => v != 0,
                _ => false,
            };
            state.set(on);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::records::ai::AiRecord;
    use crate::server::records::bi::BiRecord;
    use crate::server::records::bo::BoRecord;

    /// Each test names a distinct state — the registry is process-global, so a
    /// shared name would leak across tests run in one process.
    #[test]
    fn init_resolves_bo_state_from_out_and_write_sets_it() {
        let mut dev = DbStateDeviceSupport::new("", "@DBSTATE_TEST_BO");
        let mut rec = BoRecord::new(1);
        dev.init(&mut rec).unwrap();
        dev.write(&mut rec).unwrap();
        assert!(db_state_registry().get("DBSTATE_TEST_BO"));

        let mut rec0 = BoRecord::new(0);
        dev.write(&mut rec0).unwrap();
        assert!(!db_state_registry().get("DBSTATE_TEST_BO"));
    }

    /// bi reads the named bit into VAL and returns a computed outcome (C
    /// `return 2`, skip conversion).
    #[test]
    fn init_resolves_bi_state_from_inp_and_read_loads_it() {
        db_state_registry().set("DBSTATE_TEST_BI", true);
        let mut dev = DbStateDeviceSupport::new("@DBSTATE_TEST_BI", "");
        let mut rec = BiRecord::new(0);
        dev.init(&mut rec).unwrap();
        let outcome = dev.read(&mut rec).unwrap();
        assert!(outcome.did_compute());
        assert_eq!(rec.get_field("VAL"), Some(EpicsValue::Enum(1)));
    }

    /// C registers no `(ai, "Db State")` dset; the record-type gate Errs so
    /// `wire_device_to_record` flags the record INVALID.
    #[test]
    fn init_rejects_unsupported_record_type() {
        let mut dev = DbStateDeviceSupport::new("@X", "");
        let mut rec = AiRecord::new(0.0);
        assert!(dev.init(&mut rec).is_err());
    }

    /// An empty instio name leaves `state = None` (C `dpvt == NULL`): read is a
    /// no-op (VAL unchanged) and write is a no-op (no state touched). C's guard
    /// skips `prec->udf = FALSE` as well as the VAL write, so the read must
    /// report that it produced nothing — otherwise the framework re-derives UDF
    /// from the always-defined Enum VAL and clears an alarm C keeps raised.
    #[test]
    fn empty_instio_name_is_noop() {
        let mut dev = DbStateDeviceSupport::new("@", "");
        let mut rec = BiRecord::new(7);
        dev.init(&mut rec).unwrap();
        assert!(dev.state.is_none());
        let outcome = dev.read(&mut rec).unwrap();
        assert!(
            outcome.status.read_failed(),
            "no state: C touches neither VAL nor udf"
        );
        assert!(
            !outcome.asserts_undefined(),
            "C does not set udf either — it leaves the record's own flag alone"
        );
        assert_eq!(rec.get_field("VAL"), Some(EpicsValue::Enum(7)));
    }
}
