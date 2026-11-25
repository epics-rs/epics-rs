//! aCalc `IXNZ` thresholds at `SMALL` = 1e-9 — `fabs(a[i]) > SMALL`
//! (`aCalcPerform.c:893-898`) — not at an exact `!= 0.0`. Expectations are the
//! compiled synApps `aCalcPerform` printing `dresult` for arraySize 8.

use epics_base_rs::calc::{ArrayInputs, ArrayStackValue, acalc};

fn ixnz(aa: [f64; 8]) -> f64 {
    let mut inputs = ArrayInputs::new(8);
    inputs.arrays[0] = aa.to_vec();
    match acalc("IXNZ(AA)", &mut inputs).unwrap() {
        ArrayStackValue::Double(v) => v,
        other => panic!("IXNZ must reduce to a Double, got {other:?}"),
    }
}

/// C: 2. The 1e-12 at index 1 is below SMALL and is skipped; the 1e-8 at index 2
/// is above it and wins. An exact `!= 0.0` stops at index 1 instead.
#[test]
fn noise_below_small_is_skipped() {
    assert_eq!(ixnz([0.0, 1e-12, 1e-8, 0.0, 0.0, 0.0, 0.0, 0.0]), 2.0);
}

/// C: -1. An array that is entirely sub-SMALL noise has no non-zero element at
/// all, where an exact `!= 0.0` answers 0.
#[test]
fn an_all_noise_array_has_no_nonzero_element() {
    assert_eq!(ixnz([1e-12, 1e-12, 1e-12, 0.0, 0.0, 0.0, 0.0, 0.0]), -1.0);
}

/// C: 0. The threshold is on the MAGNITUDE, so a negative element above SMALL
/// still wins at its own index.
#[test]
fn the_threshold_is_on_the_magnitude() {
    assert_eq!(ixnz([-1e-8, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0]), 0.0);
}

/// C: 2 / -1 — the two cases where exact and thresholded tests agree, kept so a
/// future rewrite cannot regress the ordinary path.
#[test]
fn ordinary_arrays_are_unchanged() {
    assert_eq!(ixnz([0.0, 0.0, 5.0, 0.0, 0.0, 0.0, 0.0, 0.0]), 2.0);
    assert_eq!(ixnz([0.0; 8]), -1.0);
}
