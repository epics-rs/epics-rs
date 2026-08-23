// RTEMS-EXEC-MODEL-ALLOW(1): a sync test that touches no runtime.
//! A record parked at NaN must post its VALUE/LOG monitor once, not once per
//! process cycle.
//!
//! C `calcRecord.c:404` deadbands VAL through `recGblCheckDeadband(&prec->mlst,
//! ...)`, and `calcRecord.c` never seeds MLST, so a `CALC("0/0")` record
//! crosses 0 -> NaN on its first cycle and posts. From the second cycle on, C
//! compares a NaN MLST against a NaN VAL, finds `delta == 0`, adds no bit and
//! leaves MLST alone (recGbl.c:345-370). A `.1 second` record therefore sends
//! one update, not ten a second into every connected archiver.

use epics_base_rs::server::record::RecordInstance;
use epics_base_rs::server::records::calc::CalcRecord;
use epics_base_rs::types::EpicsValue;

fn parked_at_nan() -> RecordInstance {
    let mut inst = RecordInstance::new("R5:NAN".to_string(), CalcRecord::default());
    inst.record
        .put_field("MDEL", EpicsValue::Double(0.0))
        .unwrap();
    inst.record
        .put_field("ADEL", EpicsValue::Double(0.0))
        .unwrap();
    inst.record
        .put_field("VAL", EpicsValue::Double(f64::NAN))
        .unwrap();
    inst
}

#[test]
fn a_calc_parked_at_nan_posts_on_the_first_cycle_only() {
    let mut inst = parked_at_nan();

    assert_eq!(
        inst.check_deadband_ext(),
        (true, true),
        "0 -> NaN crosses the finite/NaN boundary, so C's delta is +inf"
    );
    for cycle in 2..=5 {
        assert_eq!(
            inst.check_deadband_ext(),
            (false, false),
            "cycle {cycle}: NaN has already been posted and the value has not moved"
        );
    }
}

/// The half of C's rule that makes the first half hold: MLST is written only
/// when the comparison fires (`if (delta > deadband) { ...; *poldval = newval; }`).
/// A NaN that reached MLST must stay there, or every later cycle re-crosses the
/// same boundary.
#[test]
fn the_posted_nan_is_retained_in_mlst_and_alst() {
    let mut inst = parked_at_nan();
    let _ = inst.check_deadband_ext();

    for field in ["MLST", "ALST"] {
        match inst.record.get_field(field) {
            Some(EpicsValue::Double(v)) => {
                assert!(v.is_nan(), "{field} holds {v}, so the NaN was not recorded")
            }
            other => panic!("{field} is not a double: {other:?}"),
        }
    }
}

/// A finite value still leaves NaN behind once, and only once: the record
/// recovering from NaN posts, and the cycle after that does not.
#[test]
fn recovery_from_nan_posts_once_too() {
    let mut inst = parked_at_nan();
    let _ = inst.check_deadband_ext();
    let _ = inst.check_deadband_ext();

    inst.record
        .put_field("VAL", EpicsValue::Double(4.0))
        .unwrap();
    assert_eq!(
        inst.check_deadband_ext(),
        (true, true),
        "NaN -> finite crosses the boundary the other way"
    );
    assert_eq!(
        inst.check_deadband_ext(),
        (false, false),
        "4.0 -> 4.0 with MDEL 0 is not a change"
    );
}
