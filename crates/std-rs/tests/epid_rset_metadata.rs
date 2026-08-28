//! `epidRecord`'s rset has TWO limit windows, and the port served one.
//!
//! `epidRecord.c:238-261` (graphic) and `:263-286` (control) are the same
//! switch twice: `VAL`/`HIHI`/`HIGH`/`LOW`/`LOLO`/`CVAL` take the process
//! variable's operator range `hopr`/`lopr`, `OVAL`/`P`/`I`/`D` take the
//! actuator's drive range `drvh`/`drvl`, and everything else falls to
//! `recGblGet*Double` — the field's TYPE range.
//!
//! Three answers, and the port had one cache. `CVAL` was missing from the
//! graphic list (`control` already had it), and the drive window was missing
//! from both.
//!
//! `epidRecord.c:112` NULLs nothing but `get_enum_str`, so all five numeric
//! slots are marked: every value below reaches the wire under a mark saying
//! the record supplied it.

use epics_base_rs::server::record::{Record, RecordInstance};
use epics_base_rs::types::EpicsValue;
use std_rs::records::epid::EpidRecord;

/// `recGbl.c` `getMaxRangeValues` for DBF_DOUBLE — what an unlisted field
/// takes.
const DOUBLE_RANGE: (f64, f64) = (-1e300, 1e300);

fn epid() -> RecordInstance {
    let mut rec = EpidRecord::default();
    for (name, value) in [
        ("PREC", EpicsValue::Short(4)),
        ("EGU", EpicsValue::String("K".into())),
        ("HOPR", EpicsValue::Double(50.0)),
        ("LOPR", EpicsValue::Double(0.0)),
        ("DRVH", EpicsValue::Double(8.0)),
        ("DRVL", EpicsValue::Double(-8.0)),
    ] {
        rec.put_field(name, value)
            .unwrap_or_else(|e| panic!("epid models {name}: {e:?}"));
    }
    RecordInstance::new("T:PID".to_string(), rec)
}

/// `(lower, upper)`, as the snapshot accessors return them.
fn display(inst: &RecordInstance, field: &str) -> (f64, f64) {
    inst.snapshot_for_field(field)
        .unwrap_or_else(|| panic!("{field} has no snapshot"))
        .display_limits()
        .unwrap_or_else(|| panic!("{field} serves no display limits"))
}

fn control(inst: &RecordInstance, field: &str) -> (f64, f64) {
    inst.snapshot_for_field(field)
        .unwrap_or_else(|| panic!("{field} has no snapshot"))
        .control_limits()
        .unwrap_or_else(|| panic!("{field} serves no control limits"))
}

/// The operator window. `CVAL` is the controlled-variable readback and sits in
/// it in BOTH slots — `epidRecord.c:249` and `:275`.
#[test]
fn epid_operator_window_covers_val_and_cval() {
    let inst = epid();

    for field in ["VAL", "CVAL"] {
        assert_eq!(display(&inst, field), (0.0, 50.0), "{field} display");
        assert_eq!(control(&inst, field), (0.0, 50.0), "{field} control");
    }
}

/// The drive window — the second answer, on the actuator's scale. A record has
/// one metadata cache, so these four cannot come from it.
#[test]
fn epid_drive_window_covers_oval_and_the_three_gains() {
    let inst = epid();

    for field in ["OVAL", "P", "I", "D"] {
        assert_eq!(display(&inst, field), (-8.0, 8.0), "{field} display");
        assert_eq!(control(&inst, field), (-8.0, 8.0), "{field} control");
    }
}

/// The third answer, which pins the windows as windows: a field in neither
/// takes `recGblGetGraphicDouble`, not whichever range happened to be cached.
#[test]
fn epid_unlisted_field_takes_the_type_range() {
    let inst = epid();

    assert_eq!(display(&inst, "ALST"), DOUBLE_RANGE);
    assert_eq!(control(&inst, "ALST"), DOUBLE_RANGE);
}

/// `epidRecord.c:217-223` copies `pepid->egu` with no field test, and
/// `:225-234` answers `pepid->prec` for `VAL` and `CVAL`.
#[test]
fn epid_supplies_units_and_precision_from_the_record() {
    let inst = epid();
    let snap = inst.snapshot_for_field("CVAL").expect("epid serves CVAL");

    assert_eq!(snap.units().expect("units leaf").as_str_lossy(), "K");
    assert_eq!(snap.precision(), Some(4));
}
