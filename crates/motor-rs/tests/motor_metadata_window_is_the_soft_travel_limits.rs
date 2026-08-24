//! `motor`'s VAL metadata window is HLM/LLM, not HOPR/LOPR.
//!
//! `motorRecord.cc:3213-3258` (`get_graphic_double`) and `:3263-3308`
//! (`get_control_double`) answer the soft travel limits for VAL/RBV, and the
//! record's own HOPR/LOPR never enter either switch. That makes `motor` the
//! one ported type whose display window is not the operator range. HOPR/LOPR
//! are loaded apart from HLM/LLM here so that a window sourced from the
//! operator range is visible as `(-99, 99)` rather than passing silently.
//!
//! What that catches is `metadata_override` (`field_access.rs:1845`), NOT the
//! record-level `graphic_limit_fields` / `control_limit_source` derivation in
//! `epics-base-rs`. `apply_field_metadata_override` runs after
//! `route_field_metadata`, so motor's per-field answer overwrites the
//! record-level pair on every field. Measured by deleting each helper's
//! `"motor"` arm in turn and dumping both windows for all 135 served fields:
//! byte-identical either way. The two arms are redundant with the override,
//! not dead weight to remove — dropping them would leave the record-level
//! cache serving the operator range, wrong on its own and masked only by the
//! layer above it.
//!
//! Those two C switches have FOUR arms, not one, and the other three are
//! pinned below by boundary: the dial pair, the raw pair on each side of the
//! `MRES` sign test, the velocity pair, and a field in no arm at all. Every
//! case asserts BOTH windows, because C's `get_graphic_double` and
//! `get_control_double` are the same switch written twice and only a paired
//! assertion catches an edit that changes one of them.

use epics_base_rs::server::record::{Record, RecordInstance};
use epics_base_rs::types::EpicsValue;
use motor_rs::MotorRecord;

fn loaded() -> RecordInstance {
    let mut rec = MotorRecord::new();
    for (name, value) in [
        ("EGU", EpicsValue::String("mm".into())),
        ("PREC", EpicsValue::Short(4)),
        ("HLM", EpicsValue::Double(12.5)),
        ("LLM", EpicsValue::Double(-12.5)),
        ("HOPR", EpicsValue::Double(99.0)),
        ("LOPR", EpicsValue::Double(-99.0)),
    ] {
        rec.put_field(name, value)
            .unwrap_or_else(|e| panic!("field({name}) failed to load: {e:?}"));
    }
    RecordInstance::new("T:MOTOR".to_string(), rec)
}

#[test]
fn val_serves_the_soft_travel_limits_in_both_windows() {
    let inst = loaded();
    let snap = inst.snapshot_for_field("VAL").expect("VAL has no snapshot");

    assert_eq!(snap.display_limits(), Some((-12.5, 12.5)));
    assert_eq!(snap.control_limits(), Some((-12.5, 12.5)));
}

/// The units and precision leaves of the same window still come from the
/// record: `motorRecord.cc`'s `get_units` default arm copies EGU, and its
/// `get_precision` leaves an in-range PREC standing.
#[test]
fn val_serves_egu_and_prec_from_the_record() {
    let inst = loaded();
    let snap = inst.snapshot_for_field("VAL").expect("VAL has no snapshot");

    assert_eq!(
        snap.units().map(|u| u.as_str_lossy().into_owned()),
        Some("mm".to_string())
    );
    assert_eq!(snap.precision(), Some(4));
}

/// The dial arm (`motorRecord.cc:3227-3231` / `:3278-3282`).
///
/// `OFF` is what makes this a real boundary rather than a coincidence: with
/// `DIR=Pos` and `OFF=0` the user and dial limits are numerically equal by
/// construction (`set_dial_highlimit` keeps `HLM = DHLM + OFF`), so a pass
/// that served the wrong pair would still look right. `OFF=100` separates
/// them.
fn dial_and_raw_loaded(mres: f64) -> RecordInstance {
    let mut rec = MotorRecord::new();
    for (name, value) in [
        ("EGU", EpicsValue::String("mm".into())),
        ("PREC", EpicsValue::Short(4)),
        ("OFF", EpicsValue::Double(100.0)),
        ("DHLM", EpicsValue::Double(6.0)),
        ("DLLM", EpicsValue::Double(-4.0)),
        ("MRES", EpicsValue::Double(mres)),
        ("VMAX", EpicsValue::Double(7.0)),
        ("VBAS", EpicsValue::Double(0.5)),
        ("HOPR", EpicsValue::Double(99.0)),
        ("LOPR", EpicsValue::Double(-99.0)),
    ] {
        rec.put_field(name, value)
            .unwrap_or_else(|e| panic!("field({name}) failed to load: {e:?}"));
    }
    RecordInstance::new("T:MOTOR".to_string(), rec)
}

fn windows(inst: &RecordInstance, field: &str) -> ((f64, f64), (f64, f64)) {
    let snap = inst
        .snapshot_for_field(field)
        .unwrap_or_else(|| panic!("{field} has no snapshot"));
    (
        snap.display_limits()
            .unwrap_or_else(|| panic!("{field} supplies no display limits")),
        snap.control_limits()
            .unwrap_or_else(|| panic!("{field} supplies no control limits")),
    )
}

#[test]
fn the_user_arm_is_the_user_travel_limits_even_when_the_dial_pair_differs() {
    let inst = dial_and_raw_loaded(0.5);
    // HLM/LLM = DHLM/DLLM + OFF.
    for field in ["VAL", "RBV"] {
        assert_eq!(windows(&inst, field), ((96.0, 106.0), (96.0, 106.0)));
    }
}

#[test]
fn the_dial_arm_is_the_dial_travel_limits() {
    let inst = dial_and_raw_loaded(0.5);
    for field in ["DVAL", "DRBV"] {
        assert_eq!(windows(&inst, field), ((-4.0, 6.0), (-4.0, 6.0)));
    }
}

/// The raw arm is the one that computes rather than naming a field pair:
/// `DHLM / MRES` and `DLLM / MRES` (`motorRecord.cc:3233-3245`).
#[test]
fn the_raw_arm_divides_the_dial_limits_by_a_positive_mres() {
    let inst = dial_and_raw_loaded(0.5);
    for field in ["RVAL", "RRBV"] {
        assert_eq!(windows(&inst, field), ((-8.0, 12.0), (-8.0, 12.0)));
    }
}

/// The sign boundary: at `MRES < 0` C swaps which dial limit feeds which raw
/// end (`:3240-3244`), because dividing by a negative resolution inverts the
/// order. `MRES = -0.5` against the `+0.5` case above is the same magnitude,
/// so only the swap distinguishes them.
#[test]
fn a_negative_mres_swaps_the_raw_limit_pair() {
    let inst = dial_and_raw_loaded(-0.5);
    for field in ["RVAL", "RRBV"] {
        assert_eq!(windows(&inst, field), ((-12.0, 8.0), (-12.0, 8.0)));
    }
}

/// The velocity arm (`:3247-3250` / `:3298-3301`) — the only arm whose pair is
/// neither a travel limit nor derived from one, and the only one whose lower
/// end is not a mirror of its upper.
#[test]
fn the_velocity_arm_ranges_over_vmax_and_vbas() {
    let inst = dial_and_raw_loaded(0.5);
    assert_eq!(windows(&inst, "VELO"), ((0.5, 7.0), (0.5, 7.0)));
}

/// A field in NO arm takes C's `default:` — `recGblGetGraphicDouble` /
/// `recGblGetControlDouble`, i.e. the DBF type range, NOT any of motor's four
/// pairs. `ACCL` is `DBF_DOUBLE`, so both windows are the double range.
#[test]
fn a_field_in_no_arm_falls_back_to_the_type_range() {
    let inst = dial_and_raw_loaded(0.5);
    assert_eq!(windows(&inst, "ACCL"), ((-1e300, 1e300), (-1e300, 1e300)));
}
