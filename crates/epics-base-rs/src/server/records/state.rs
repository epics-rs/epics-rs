use crate::error::{CaError, CaResult};
use crate::server::database::AsyncDbHandle;
use crate::server::record::{ProcessOutcome, Record};
use crate::types::{EpicsValue, PvString};

/// `VAL`/`OVAL` are `size(20)` `char[20]` buffers in C
/// (`stateRecord.dbd.pod`): 19 payload bytes + an implicit NUL.
const STATE_SIZE: usize = 20;

fn truncate_bytes(s: PvString, max: usize) -> PvString {
    if s.len() <= max {
        return s;
    }
    PvString::from_bytes(s.as_bytes()[..max].to_vec())
}

/// `state` record — a place for a state program to publish an ASCII string
/// for an operator interface (C `stateRecord.c`).
///
/// `process()` posts `VAL` only when it differs from `OVAL`, then copies
/// `VAL` into the `SPC_NOMOD` `OVAL` tracker and scans the forward link
/// (`stateRecord.c:96-129`). The on-change post is wired through the
/// framework's value-change gate ([`Record::monitor_value_changed`]) — the
/// record has no MDEL/ADEL deadband, matching C's pure
/// `strncmp(oval, val)` test.
///
/// **Deprecated** in upstream EPICS (slated for removal in EPICS 7.1; replace
/// with a `stringin`). C `init_record` pass 0 prints a deprecation warning;
/// the port mirrors it in [`Record::init_record`].
pub struct StateRecord {
    /// `VAL` — the published state string (`DBF_STRING`, `size(20)`,
    /// `pp(TRUE)`).
    pub val: PvString,
    /// `OVAL` — previously posted value (`DBF_STRING`, `SPC_NOMOD`),
    /// the change tracker for `VAL` monitors.
    pub oval: PvString,
    /// Record name, delivered by the framework at `add_record`
    /// ([`Record::set_async_context`]); used to name the deprecation
    /// warning exactly as C does.
    name: Option<String>,
}

impl Default for StateRecord {
    fn default() -> Self {
        Self {
            val: PvString::new(),
            oval: PvString::new(),
            name: None,
        }
    }
}

impl StateRecord {
    pub fn new(val: &str) -> Self {
        Self {
            val: truncate_bytes(PvString::from(val), STATE_SIZE - 1),
            ..Default::default()
        }
    }
}

impl Record for StateRecord {
    fn record_type(&self) -> &'static str {
        "state"
    }

    /// C `stateRecord.c::init_record` pass 0 warns that the record type is
    /// deprecated (`stateRecord.c:96-104`). Mirror the message, naming the
    /// record as C does (`prec->name`).
    fn init_record(&mut self, pass: u8) -> CaResult<()> {
        if pass == 0 {
            let name = self.name.as_deref().unwrap_or("<unknown>");
            eprintln!(
                "WARNING: Using deprecated record type \"state\" for record \"{name}\".\n\
                 This record type will be removed beginning with EPICS 7.1. Please replace it\n\
                 by a stringin record."
            );
        }
        Ok(())
    }

    fn set_async_context(&mut self, name: String, _db: AsyncDbHandle) {
        self.name = Some(name);
    }

    /// C `stateRecord.c::monitor` (lines 120-129): post `VAL` when
    /// `strncmp(oval, val)` differs, then copy `VAL` into `OVAL`. The change
    /// is captured before the copy so the framework's VAL-post gate reads
    /// the C result.
    fn process(&mut self) -> CaResult<ProcessOutcome> {
        Ok(ProcessOutcome::complete())
    }

    /// No MDEL/ADEL deadband — `VAL` posts on a pure change, matching C
    /// (`strncmp(oval, val)`).
    fn uses_monitor_deadband(&self) -> bool {
        false
    }

    /// C `stateRecord.c:115-118` `monitor()`: the `strncmp(oval, val)` mismatch both
    /// raises `DBE_VALUE | DBE_LOG` and copies VAL into OVAL. Compared and
    /// committed HERE, at C's position, never captured during `process()`.
    fn monitor_value_changed(&mut self) -> Option<bool> {
        let changed = self.oval != self.val;
        if changed {
            self.oval = self.val.clone();
        }
        Some(changed)
    }

    /// C `stateRecord.c::monitor` (lines 120-129) posts only `VAL`. `OVAL`
    /// is a `SPC_NOMOD` tracker C never posts (no `db_post_events` call, and
    /// `dbPut`'s written-field post can't reach a `SPC_NOMOD` field).
    /// Declaring it here excludes it from the framework's generic
    /// subscribed-field change loop (`processing.rs:2439`), so a client
    /// subscribed to `.OVAL` receives only the one-shot value at subscribe —
    /// never a change update — matching C. It stays readable on the GET path.
    fn event_posted_fields(&self) -> &'static [&'static str] {
        &["OVAL"]
    }

    fn get_field(&self, name: &str) -> Option<EpicsValue> {
        match name {
            "VAL" => Some(EpicsValue::String(self.val.clone())),
            "OVAL" => Some(EpicsValue::String(self.oval.clone())),
            _ => None,
        }
    }

    fn put_field(&mut self, name: &str, value: EpicsValue) -> CaResult<()> {
        match name {
            "VAL" => match value {
                EpicsValue::String(s) => self.val = truncate_bytes(s, STATE_SIZE - 1),
                _ => return Err(CaError::TypeMismatch("VAL".into())),
            },
            "OVAL" => match value {
                EpicsValue::String(s) => self.oval = truncate_bytes(s, STATE_SIZE - 1),
                _ => return Err(CaError::TypeMismatch("OVAL".into())),
            },
            _ => return Err(CaError::FieldNotFound(name.to_string())),
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `process()` copies VAL into OVAL and reports the change via
    /// `monitor_value_changed()`, then reports no change on a repeat.
    #[test]
    fn process_posts_on_change_then_quiesces() {
        let mut rec = StateRecord::default();
        rec.put_field("VAL", EpicsValue::String("Idle".into()))
            .unwrap();
        rec.process().unwrap();
        assert_eq!(rec.monitor_value_changed(), Some(true));
        assert_eq!(
            rec.get_field("OVAL"),
            Some(EpicsValue::String("Idle".into()))
        );
        rec.process().unwrap();
        assert_eq!(rec.monitor_value_changed(), Some(false));
    }

    /// VAL is `size(20)` in C: a longer string is truncated to 19 bytes.
    #[test]
    fn val_truncated_to_field_size() {
        let mut rec = StateRecord::default();
        rec.put_field("VAL", EpicsValue::String("y".repeat(40).into()))
            .unwrap();
        match rec.get_field("VAL") {
            Some(EpicsValue::String(s)) => assert_eq!(s.len(), 19),
            other => panic!("expected String, got {other:?}"),
        }
    }
}
