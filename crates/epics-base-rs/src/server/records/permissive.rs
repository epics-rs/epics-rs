use crate::error::{CaError, CaResult};
use crate::server::record::{ProcessOutcome, Record};
use crate::types::{EpicsValue, PvString};

/// `LABL` is a `size(20)` `char[20]` in C (`permissiveRecord.dbd.pod`):
/// the button label holds 19 payload bytes + an implicit NUL.
const LABL_SIZE: usize = 20;

fn truncate_bytes(s: PvString, max: usize) -> PvString {
    if s.len() <= max {
        return s;
    }
    PvString::from_bytes(s.as_bytes()[..max].to_vec())
}

/// `permissive` record — operator-permission gate between a server and a
/// client (C `permissiveRecord.c`).
///
/// Two `DBF_USHORT` flags carry the protocol: `VAL` ("Status") and `WFLG`
/// ("Wait Flag"). `process()` posts `VAL` when it differs from `OVAL` and
/// `WFLG` when it differs from `OFLG`, both with `DBE_VALUE | DBE_LOG`, then
/// copies the new values into the `SPC_NOMOD` old-value trackers and scans
/// the forward link (`permissiveRecord.c:81-117`).
///
/// Monitor wiring: `VAL` posts via the framework's value-change gate
/// ([`Record::monitor_value_changed`]) — the record has no MDEL/ADEL
/// deadband, matching C's pure `oval != val` test. `WFLG` is a subscribed
/// auxiliary field, so the framework's generic change-detection loop posts
/// it on change with the same `DBE_VALUE | DBE_LOG` mask. `OVAL`/`OFLG`
/// mirror C's post-`monitor()` tracker state for a `caget`.
///
/// **Deprecated** in upstream EPICS, but still part of the base record set.
pub struct PermissiveRecord {
    /// `VAL` — "Status" (`DBF_USHORT`, `pp(TRUE)`).
    pub val: u16,
    /// `OVAL` — "Old Status" (`DBF_USHORT`, `SPC_NOMOD`). Tracks the
    /// previously posted `VAL`.
    pub oval: u16,
    /// `WFLG` — "Wait Flag" (`DBF_USHORT`, `pp(TRUE)`).
    pub wflg: u16,
    /// `OFLG` — "Old Flag" (`DBF_USHORT`, `SPC_NOMOD`). Tracks the
    /// previously posted `WFLG`.
    pub oflg: u16,
    /// `LABL` — "Button Label" (`DBF_STRING`, `size(20)`, `pp(TRUE)`).
    pub labl: PvString,
}

impl Default for PermissiveRecord {
    fn default() -> Self {
        Self {
            val: 0,
            oval: 0,
            wflg: 0,
            oflg: 0,
            labl: PvString::new(),
        }
    }
}

impl Record for PermissiveRecord {
    fn record_type(&self) -> &'static str {
        "permissive"
    }

    /// C `permissiveRecord.c::monitor` (lines 90-117): post `VAL` when
    /// `oval != val` and `WFLG` when `oflg != wflg`, then copy the new
    /// values into `OVAL`/`OFLG`. The change is captured here (before the
    /// `oval` copy) so the framework's VAL-post gate reads the C result;
    /// `WFLG` posts through the generic subscribed-field loop.
    fn process(&mut self) -> CaResult<ProcessOutcome> {
        self.oflg = self.wflg;
        Ok(ProcessOutcome::complete())
    }

    /// No MDEL/ADEL deadband — `VAL` posts on a pure change, matching C
    /// (`oval != val`).
    fn uses_monitor_deadband(&self) -> bool {
        false
    }

    /// C `permissiveRecord.c:107-110` `monitor()` posts VAL on the `oval !=
    /// val` it snapshotted at `:100-103`, and the copy `prec->oval = val`
    /// at `:105` is UNCONDITIONAL. Compared and committed HERE, at C's
    /// position. (`oflg` is copied in the same C function; the port still
    /// rolls it in `process()`, because WFLG posts through the generic
    /// subscribed-field loop rather than through this gate.)
    fn monitor_value_changed(&mut self) -> Option<bool> {
        let changed = self.oval != self.val;
        self.oval = self.val;
        Some(changed)
    }

    /// C `permissiveRecord.c::monitor` (lines 90-117) posts only `VAL` and
    /// `WFLG`. `OVAL`/`OFLG` are `SPC_NOMOD` trackers C never posts: no
    /// record code calls `db_post_events` on them, and `dbPut`'s
    /// written-field post (`dbAccess.c:1407-1414`) can't reach a `SPC_NOMOD`
    /// field. Declaring them here excludes them from the framework's generic
    /// subscribed-field change loop (`processing.rs:2439`), so a client
    /// subscribed to `.OVAL`/`.OFLG` receives only the one-shot value at
    /// subscribe — never a change update — matching C. They stay readable on
    /// the GET path.
    fn event_posted_fields(&self) -> &'static [&'static str] {
        &["OVAL", "OFLG"]
    }

    fn get_field(&self, name: &str) -> Option<EpicsValue> {
        match name {
            "VAL" => Some(EpicsValue::UShort(self.val)),
            "OVAL" => Some(EpicsValue::UShort(self.oval)),
            "WFLG" => Some(EpicsValue::UShort(self.wflg)),
            "OFLG" => Some(EpicsValue::UShort(self.oflg)),
            "LABL" => Some(EpicsValue::String(self.labl.clone())),
            _ => None,
        }
    }

    fn put_field(&mut self, name: &str, value: EpicsValue) -> CaResult<()> {
        match name {
            "VAL" => self.val = ushort_of(name, value)?,
            "OVAL" => self.oval = ushort_of(name, value)?,
            "WFLG" => self.wflg = ushort_of(name, value)?,
            "OFLG" => self.oflg = ushort_of(name, value)?,
            "LABL" => match value {
                EpicsValue::String(s) => self.labl = truncate_bytes(s, LABL_SIZE - 1),
                _ => return Err(CaError::TypeMismatch("LABL".into())),
            },
            _ => return Err(CaError::FieldNotFound(name.to_string())),
        }
        Ok(())
    }
}

/// Coerce a put value to `u16`, accepting the `DBF_USHORT` wire type plus
/// the `Short`/`Long` an internal/db-load put may carry.
fn ushort_of(field: &str, value: EpicsValue) -> CaResult<u16> {
    match value {
        EpicsValue::UShort(v) => Ok(v),
        EpicsValue::Short(v) => Ok(v as u16),
        EpicsValue::Long(v) => Ok(v as u16),
        EpicsValue::Enum(v) => Ok(v),
        _ => Err(CaError::TypeMismatch(field.into())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::record::RecordInstance;

    /// A cycle copies VAL into OVAL and WFLG into OFLG, mirroring C
    /// `monitor()`'s tracker update (`permissiveRecord.c:105-106`). OVAL is
    /// committed by `monitor_value_changed`, which IS that function; OFLG
    /// still rolls in `process()` because WFLG posts through the generic
    /// subscribed-field loop.
    #[test]
    fn process_copies_trackers() {
        let mut rec = PermissiveRecord::default();
        rec.put_field("VAL", EpicsValue::UShort(1)).unwrap();
        rec.put_field("WFLG", EpicsValue::UShort(1)).unwrap();
        rec.process().unwrap();
        rec.monitor_value_changed();
        assert_eq!(rec.get_field("OVAL"), Some(EpicsValue::UShort(1)));
        assert_eq!(rec.get_field("OFLG"), Some(EpicsValue::UShort(1)));
    }

    /// `monitor_value_changed()` reports the C `oval != val` result captured
    /// before the tracker copy: true on a changed cycle, false on an
    /// unchanged one.
    #[test]
    fn value_changed_gate_tracks_oval() {
        let mut rec = PermissiveRecord::default();
        rec.put_field("VAL", EpicsValue::UShort(7)).unwrap();
        rec.process().unwrap();
        assert_eq!(rec.monitor_value_changed(), Some(true));
        // A second process with no VAL change reports no change.
        rec.process().unwrap();
        assert_eq!(rec.monitor_value_changed(), Some(false));
    }

    /// LABL is `size(20)` in C: a longer label is truncated to 19 bytes.
    #[test]
    fn labl_truncated_to_field_size() {
        let mut rec = PermissiveRecord::default();
        rec.put_field("LABL", EpicsValue::String("x".repeat(40).into()))
            .unwrap();
        match rec.get_field("LABL") {
            Some(EpicsValue::String(s)) => assert_eq!(s.len(), 19),
            other => panic!("expected String, got {other:?}"),
        }
    }

    /// VAL is `DBF_USHORT`, served as an unsigned scalar.
    #[test]
    fn val_snapshot_is_ushort() {
        let mut rec = PermissiveRecord::default();
        rec.put_field("VAL", EpicsValue::UShort(3)).unwrap();
        let inst = RecordInstance::new("PERM:VAL".into(), rec);
        let snap = inst.snapshot_for_field("VAL").unwrap();
        assert_eq!(snap.value, EpicsValue::UShort(3));
    }
}
