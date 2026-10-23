use crate::error::{CaError, CaResult};
use crate::server::record::{CycleCell, CycleMarks, CyclePostMask, CycleRead, CycleValue};
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
/// deadband, matching C's pure `oval != val` test. `WFLG` posts on C's own
/// `oflg != wflg`, not on a compare against what a client was last sent: a
/// `dbpf WFLG 1` is posted by `dbPut` and again by the `monitor()` it
/// processes. `OVAL`/`OFLG` mirror C's post-`monitor()` tracker state.
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
    /// The last `monitor()` found `oflg != wflg`; only it sets this, so a
    /// put-path drain cannot post C's monitor-time WFLG.
    wflg_marked: bool,
}

impl Default for PermissiveRecord {
    fn default() -> Self {
        Self {
            val: 0,
            oval: 0,
            wflg: 0,
            oflg: 0,
            labl: PvString::new(),
            wflg_marked: false,
        }
    }
}

impl Record for PermissiveRecord {
    crate::slot_map!(crate::server::record::dbd_generated::slots::permissive;
        fields:
        VAL => val,
        WFLG => wflg,
        OVAL => oval,
        OFLG => oflg;
        custom: LABL;
        spill:);

    fn acts_before_process(&self) -> bool {
        false
    }

    fn cycle_hooks(&self) -> crate::server::record::CycleHooks {
        crate::server::record::CycleHooks::NONE
    }

    /// `monitor()` posts VAL and WFLG alone (`permissiveRecord.c:90-117`).
    fn process_posted_fields(&self) -> Option<crate::server::record::DeclaredFields> {
        Some(
            &const {
                crate::server::record::declared_set(
                    crate::server::record::dbd_generated::PERMISSIVE_FIELDS,
                    ["VAL", "WFLG"],
                )
            },
        )
    }

    fn record_type(&self) -> &'static str {
        "permissive"
    }

    fn process(&mut self) -> CaResult<ProcessOutcome> {
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
    /// position.
    fn monitor_value_changed(&mut self) -> Option<bool> {
        let changed = self.oval != self.val;
        self.oval = self.val;
        Some(changed)
    }

    /// C's `oflg != wflg` (`:111`) is the WFLG mark, compared and committed
    /// (`:106`) beside `oval`. Unmarked, `wflg_marked` is already false: the
    /// take that `marked` guarantees clears it.
    fn monitor_value_classes(
        &mut self,
        _gate: crate::server::record::MonitorValueGate,
        _last_posted: &mut [f64; 2],
    ) -> crate::server::record::MonitorValueClasses {
        let changed = self.monitor_value_changed() == Some(true);
        let classes = crate::server::record::MonitorValueClasses {
            value: changed,
            archive: changed,
            hash_changed: false,
            marked: false,
        };
        if self.oflg == self.wflg {
            return classes;
        }
        self.oflg = self.wflg;
        self.wflg_marked = true;
        crate::server::record::MonitorValueClasses {
            marked: true,
            ..classes
        }
    }

    /// `permissiveRecord.c:111-114`: `monitor_mask|DBE_VALUE|DBE_LOG`.
    fn take_cycle_posted_fields(&mut self) -> CycleMarks {
        const WFLG_POST_NAMED: [(&str, CyclePostMask); 1] =
            [("WFLG", CyclePostMask::MonitorValueLog)];
        static WFLG_POST: [(
            crate::server::record::FieldIx,
            crate::server::record::CyclePostMask,
        ); 1] = crate::server::record::declared_post_table(
            crate::server::record::dbd_generated::PERMISSIVE_FIELDS,
            WFLG_POST_NAMED,
        );
        CycleMarks::of(&WFLG_POST, std::mem::take(&mut self.wflg_marked) as u128)
    }

    fn fields_posted_only_when_marked(&self) -> &'static [&'static str] {
        &["WFLG"]
    }

    /// The `recGblResetAlarms` mask rides the `oval != val` post only.
    fn alarm_alone_posts_value(&self) -> bool {
        false
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

    fn cycle_cell(&self, cell: CycleCell) -> CycleRead {
        match cell {
            CycleCell::Val => CycleRead::Value(CycleValue::UShort(self.val)),
            CycleCell::Oval => CycleRead::Value(CycleValue::UShort(self.oval)),
            _ => CycleRead::Absent,
        }
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
    /// `monitor()`'s tracker update (`permissiveRecord.c:105-106`): OVAL in
    /// `monitor_value_classes`.
    #[test]
    fn process_copies_trackers() {
        let mut rec = PermissiveRecord::default();
        rec.put_field("VAL", EpicsValue::UShort(1)).unwrap();
        rec.put_field("WFLG", EpicsValue::UShort(1)).unwrap();
        rec.process().unwrap();
        let mut last_posted = [0.0; 2];
        let gate = crate::server::record::MonitorValueGate {
            uses_deadband: false,
            array_monitor_post: false,
        };
        let classes = rec.monitor_value_classes(gate, &mut last_posted);
        assert!(classes.marked);
        rec.take_cycle_posted_fields();
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
        let inst = RecordInstance::new("PERM:VAL", rec);
        let snap = inst.snapshot_for_field("VAL").unwrap();
        assert_eq!(snap.value, EpicsValue::UShort(3));
    }
}
