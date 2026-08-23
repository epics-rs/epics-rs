//! The derived record-level metadata cache serves what the hand-written
//! `match rtype` arms served.
//!
//! `populate_display_info` and `populate_control_info` used to name thirteen
//! record types in five and five arms; both now read
//! `Record::property_support()` and two small per-type tables
//! (`graphic_limit_fields`, `control_limit_source`). Deriving the supply from
//! the declaration is what closes the drift between them, but it is only a fix
//! if the thirteen types the arms DID name still serve the same wire values —
//! otherwise a list defect is traded for a regression on the types that were
//! already right.
//!
//! One case per arm, at the boundary where the arm's own choice decides the
//! answer: `ao`'s DRVH/DRVL against its HOPR/LOPR, `longout`'s DRVH>DRVL test,
//! `motor`'s HLM/LLM (in `motor-rs`, which owns that record), and the integer
//! arm's precision, which the arm hard-coded to 0 and the declaration says is
//! not supplied at all.

use epics_base_rs::server::record::{Record, RecordInstance};
use epics_base_rs::server::records::ai::AiRecord;
use epics_base_rs::server::records::ao::AoRecord;
use epics_base_rs::server::records::calc::CalcRecord;
use epics_base_rs::server::records::calcout::CalcoutRecord;
use epics_base_rs::server::records::compress::CompressRecord;
use epics_base_rs::server::records::int64in::Int64inRecord;
use epics_base_rs::server::records::int64out::Int64outRecord;
use epics_base_rs::server::records::longin::LonginRecord;
use epics_base_rs::server::records::longout::LongoutRecord;
use epics_base_rs::server::records::waveform::{ArrayKind, WaveformRecord};
use epics_base_rs::types::EpicsValue;

/// A `.db` record with its metadata fields set, through the two homes the port
/// has: the record's own cell where it models one, the `declared_overrides`
/// tail where it does not.
fn load<R: Record + 'static>(mut rec: R, fields: &[(&str, EpicsValue)]) -> RecordInstance {
    let mut unmodeled = Vec::new();
    for (name, value) in fields {
        if rec.put_field(name, value.clone()).is_err() {
            unmodeled.push((*name, value.clone()));
        }
    }
    let mut inst = RecordInstance::new("T:REC".to_string(), rec);
    for (name, value) in unmodeled {
        inst.put_common_field_db_load(name, value)
            .unwrap_or_else(|e| panic!("field({name}) failed to load: {e:?}"));
    }
    inst
}

/// EGU/PREC/HOPR/LOPR, the four the display arms read, at values no default
/// coincides with.
fn operator_fields() -> Vec<(&'static str, EpicsValue)> {
    vec![
        ("EGU", EpicsValue::String("mm".into())),
        ("PREC", EpicsValue::Short(3)),
        ("HOPR", EpicsValue::Double(10.0)),
        ("LOPR", EpicsValue::Double(-10.0)),
    ]
}

/// EGU and the operator range for the integer scalars, whose HOPR/LOPR follow
/// the record's own value type. `PREC` is absent because the field is:
/// `longinRecord.dbd.pod` and its three siblings declare none, so the arm's
/// literal `precision: 0` described a cell that does not exist.
fn integer_operator_fields(hopr: EpicsValue, lopr: EpicsValue) -> Vec<(&'static str, EpicsValue)> {
    vec![
        ("EGU", EpicsValue::String("mm".into())),
        ("HOPR", hopr),
        ("LOPR", lopr),
    ]
}

fn long_fields() -> Vec<(&'static str, EpicsValue)> {
    integer_operator_fields(EpicsValue::Long(10), EpicsValue::Long(-10))
}

fn int64_fields() -> Vec<(&'static str, EpicsValue)> {
    integer_operator_fields(EpicsValue::Int64(10), EpicsValue::Int64(-10))
}

fn units(inst: &RecordInstance, field: &str) -> String {
    inst.snapshot_for_field(field)
        .unwrap_or_else(|| panic!("{field} has no snapshot"))
        .units()
        .unwrap_or_else(|| panic!("{field} serves no units leaf"))
        .as_str_lossy()
        .into_owned()
}

fn precision(inst: &RecordInstance, field: &str) -> Option<i16> {
    inst.snapshot_for_field(field)
        .unwrap_or_else(|| panic!("{field} has no snapshot"))
        .precision()
}

/// `(lower, upper)`.
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

/// The `"ai" | "ao" | "calc" | "calcout"` display arm: EGU, PREC and the
/// operator range, and — for the three inputs — the operator range again as
/// control limits.
#[test]
fn the_analog_arm_still_serves_egu_prec_and_the_operator_range() {
    for (rtype, inst) in [
        ("ai", load(AiRecord::default(), &operator_fields())),
        ("calc", load(CalcRecord::default(), &operator_fields())),
        (
            "calcout",
            load(CalcoutRecord::default(), &operator_fields()),
        ),
    ] {
        assert_eq!(units(&inst, "VAL"), "mm", "{rtype} units");
        assert_eq!(precision(&inst, "VAL"), Some(3), "{rtype} precision");
        assert_eq!(display(&inst, "VAL"), (-10.0, 10.0), "{rtype} display");
        assert_eq!(control(&inst, "VAL"), (-10.0, 10.0), "{rtype} control");
    }
}

/// `ao` shares the display arm but not the control one: `aoRecord.c:356-357`
/// answers DRVH/DRVL unconditionally, so the two windows must differ on the
/// same record.
#[test]
fn ao_control_limits_are_the_drive_range_not_the_operator_range() {
    let mut fields = operator_fields();
    fields.push(("DRVH", EpicsValue::Double(8.0)));
    fields.push(("DRVL", EpicsValue::Double(-8.0)));
    let inst = load(AoRecord::default(), &fields);

    assert_eq!(units(&inst, "VAL"), "mm");
    assert_eq!(precision(&inst, "VAL"), Some(3));
    assert_eq!(display(&inst, "VAL"), (-10.0, 10.0));
    assert_eq!(control(&inst, "VAL"), (-8.0, 8.0));
}

/// The `"longin" | "longout" | "int64in" | "int64out"` display arm wrote a
/// literal `precision: 0`, which no wire ever carried: all four
/// `#define get_precision NULL`, so the slot is unsupplied and
/// `Snapshot::precision` answers `None`. EGU and the operator range are the
/// arm's real contribution.
#[test]
fn the_integer_arm_serves_egu_and_the_operator_range_and_no_precision_leaf() {
    for (rtype, inst) in [
        ("longin", load(LonginRecord::default(), &long_fields())),
        ("int64in", load(Int64inRecord::default(), &int64_fields())),
    ] {
        assert_eq!(units(&inst, "VAL"), "mm", "{rtype} units");
        assert_eq!(precision(&inst, "VAL"), None, "{rtype} precision");
        assert_eq!(display(&inst, "VAL"), (-10.0, 10.0), "{rtype} display");
        assert_eq!(control(&inst, "VAL"), (-10.0, 10.0), "{rtype} control");
    }
}

/// `longoutRecord.c:282-287` / `int64outRecord.c:265-270` take DRVH/DRVL only
/// when `DRVH > DRVL`, and the operator range otherwise — the one arm whose
/// answer depends on the field VALUES rather than the record type.
#[test]
fn the_integer_output_control_window_switches_on_drvh_above_drvl() {
    let long_drive = |drvh: i32, drvl: i32| {
        let mut f = long_fields();
        f.push(("DRVH", EpicsValue::Long(drvh)));
        f.push(("DRVL", EpicsValue::Long(drvl)));
        f
    };
    let int64_drive = |drvh: i64, drvl: i64| {
        let mut f = int64_fields();
        f.push(("DRVH", EpicsValue::Int64(drvh)));
        f.push(("DRVL", EpicsValue::Int64(drvl)));
        f
    };

    for (rtype, inst) in [
        (
            "longout",
            load(LongoutRecord::default(), &long_drive(8, -8)),
        ),
        (
            "int64out",
            load(Int64outRecord::default(), &int64_drive(8, -8)),
        ),
    ] {
        assert_eq!(units(&inst, "VAL"), "mm", "{rtype} units");
        assert_eq!(precision(&inst, "VAL"), None, "{rtype} precision");
        assert_eq!(display(&inst, "VAL"), (-10.0, 10.0), "{rtype} display");
        assert_eq!(control(&inst, "VAL"), (-8.0, 8.0), "{rtype} drive window");
    }

    // DRVH == DRVL is the unset drive range: C falls back to HOPR/LOPR.
    for (rtype, inst) in [
        ("longout", load(LongoutRecord::default(), &long_drive(0, 0))),
        (
            "int64out",
            load(Int64outRecord::default(), &int64_drive(0, 0)),
        ),
    ] {
        assert_eq!(
            control(&inst, "VAL"),
            (-10.0, 10.0),
            "{rtype} fallback window"
        );
    }
}

/// The `"waveform" | "aai" | "aao"` arm. `subArray`, the fourth kind sharing
/// their rset shape, was never in it — the gap this derivation closes — so it
/// is pinned alongside them here.
#[test]
fn the_array_arm_still_serves_egu_prec_and_the_operator_range() {
    for kind in [
        ArrayKind::Waveform,
        ArrayKind::Aai,
        ArrayKind::Aao,
        ArrayKind::SubArray,
    ] {
        let mut fields = operator_fields();
        // menuFtype is declared in DBF_ code order: 10 is DBF_DOUBLE, the
        // element type whose VAL case copies EGU rather than breaking out.
        fields.push(("FTVL", EpicsValue::Short(10)));
        let inst = load(WaveformRecord::with_kind(kind), &fields);
        let rtype = kind.as_record_type();

        assert_eq!(units(&inst, "VAL"), "mm", "{rtype} units");
        assert_eq!(precision(&inst, "VAL"), Some(3), "{rtype} precision");
        assert_eq!(display(&inst, "VAL"), (-10.0, 10.0), "{rtype} display");
        assert_eq!(control(&inst, "VAL"), (-10.0, 10.0), "{rtype} control");
    }
}

/// The `"compress"` arm. Its VAL is DBF_NOACCESS in the dbd and retyped by
/// `cvt_dbaddr`, so it reaches the units slot through
/// `compressRecord.c:453-454`'s explicit `indexof(VAL)` case rather than the
/// DBF_DOUBLE gate.
#[test]
fn the_compress_arm_still_serves_egu_prec_and_the_operator_range() {
    let inst = load(CompressRecord::default(), &operator_fields());

    assert_eq!(units(&inst, "VAL"), "mm");
    assert_eq!(precision(&inst, "VAL"), Some(3));
    assert_eq!(display(&inst, "VAL"), (-10.0, 10.0));
    assert_eq!(control(&inst, "VAL"), (-10.0, 10.0));
}
