//! aCalc `IXZ` is the real (fractional) index of the first zero CROSSING
//! (`aCalcPerform.c:879-892`), not the integer index of the first exactly-zero
//! element — that reading is C's `#if 0` dead code. Every expectation below is
//! the compiled synApps `aCalcPerform` printing `dresult` for arraySize 8.

use epics_base_rs::calc::{ArrayInputs, ArrayStackValue, acalc};

fn ixz(aa: [f64; 8]) -> f64 {
    let mut inputs = ArrayInputs::new(8);
    inputs.arrays[0] = aa.to_vec();
    match acalc("IXZ(AA)", &mut inputs).unwrap() {
        ArrayStackValue::Double(v) => v,
        other => panic!("IXZ must reduce to a Double, got {other:?}"),
    }
}

/// C: 1.5. The crossing is between a[1]=1 and a[2]=-1, so j=1 and the
/// fractional part is |1| / |1 - (-1)| = 0.5. The old exact-zero reading found
/// a[4]==0.0 and answered 4.
#[test]
fn crossing_is_interpolated_between_the_straddling_elements() {
    assert_eq!(ixz([2.0, 1.0, -1.0, -2.0, 0.0, 0.0, 0.0, 0.0]), 1.5);
}

/// C: 1.4. An asymmetric straddle — j=1, |−2| / |−2 − 3| = 0.4 — pins that the
/// interpolation is a real division and not a midpoint.
#[test]
fn crossing_interpolation_is_not_a_midpoint() {
    let v = ixz([-1.0, -2.0, 3.0, 4.0, 5.0, 6.0, 7.0, 8.0]);
    assert!((v - 1.4).abs() < 1e-12, "expected C's 1.4, got {v}");
}

/// C: -1. A waveform that never crosses zero has no answer, which is the normal
/// case for an all-positive array — the very case the exact-zero reading also
/// answered -1 for, but by accident.
#[test]
fn no_crossing_is_minus_one() {
    assert_eq!(ixz([1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0, 8.0]), -1.0);
}

/// C: -1. An all-zero array never changes its `>0` verdict, so it does not
/// cross — where the exact-zero reading answered 0.
#[test]
fn all_zeros_do_not_cross() {
    assert_eq!(ixz([0.0; 8]), -1.0);
}

/// C: 2. An exact zero at index 2 lands on an integer answer anyway (j=1,
/// d = |1|/|1-0| = 1), because C's sign test groups 0 with the negatives.
#[test]
fn an_exact_zero_still_lands_on_its_own_index() {
    assert_eq!(ixz([2.0, 1.0, 0.0, -2.0, -3.0, -4.0, -5.0, -6.0]), 2.0);
}

/// C: 0. A leading zero makes a[0] "not > 0", so a[1]=1 crosses immediately:
/// j=0 and d = |0| / |0-1| = 0.
#[test]
fn a_leading_zero_crosses_at_index_zero() {
    assert_eq!(ixz([0.0, 1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0]), 0.0);
}
