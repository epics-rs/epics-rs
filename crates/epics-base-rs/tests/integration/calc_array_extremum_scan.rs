//! aCalc's four extremum reductions — `AMAX`, `AMIN`, `IXMAX`, `IXMIN` — are one
//! C loop shape (`aCalcPerform.c:836-861`): seed value AND index from the first
//! element, advance only on a strict comparison. Ground truth below is the
//! compiled synApps `aCalcPerform` (arraySize 8).

use epics_base_rs::calc::{ArrayInputs, ArrayStackValue, acalc};

fn eval(expr: &str, aa: &[f64]) -> f64 {
    let mut inputs = ArrayInputs::new(aa.len());
    inputs.arrays[0] = aa.to_vec();
    match acalc(expr, &mut inputs).unwrap() {
        ArrayStackValue::Double(v) => v,
        other => panic!("{expr}: expected a Double result, got {other:?}"),
    }
}

/// C: `IXMAX([5,3,5,...])` = 0. A strict `>` never displaces an equal running
/// maximum, so the FIRST of the tied maxima wins. `Iterator::max_by` returns the
/// last, which is what this pins against.
#[test]
fn ixmax_ties_keep_the_first_maximum() {
    assert_eq!(
        eval("IXMAX(AA)", &[5.0, 3.0, 5.0, 0.0, 0.0, 0.0, 0.0, 0.0]),
        0.0
    );
}

/// C: `IXMIN([-5,3,-5,...])` = 0, by the mirror-image strict `<`.
#[test]
fn ixmin_ties_keep_the_first_minimum() {
    assert_eq!(
        eval("IXMIN(AA)", &[-5.0, 3.0, -5.0, 0.0, 0.0, 0.0, 0.0, 0.0]),
        0.0
    );
}

/// C: `AMAX([NaN,3,5,...])` = NaN, `IXMAX` = 0. The seed is `a[0]`, and every
/// comparison against a NaN seed is false, so nothing ever displaces it.
/// `fold(NEG_INFINITY, f64::max)` discards the NaN and answers 5 instead.
#[test]
fn nan_seed_wins_every_extremum() {
    let aa = [f64::NAN, 3.0, 5.0, 1.0, 1.0, 1.0, 1.0, 1.0];
    assert!(
        eval("AMAX(AA)", &aa).is_nan(),
        "AMAX must keep the NaN seed"
    );
    assert!(
        eval("AMIN(AA)", &aa).is_nan(),
        "AMIN must keep the NaN seed"
    );
    assert_eq!(eval("IXMAX(AA)", &aa), 0.0);
    assert_eq!(eval("IXMIN(AA)", &aa), 0.0);
}

/// C: a NaN that is NOT the seed loses every strict comparison and is skipped —
/// `AMAX([3,NaN,5,1..])` = 5, `AMIN` = 1, and the indices point at the real
/// winners.
#[test]
fn nan_after_the_seed_is_skipped() {
    let aa = [3.0, f64::NAN, 5.0, 1.0, 1.0, 1.0, 1.0, 1.0];
    assert_eq!(eval("AMAX(AA)", &aa), 5.0);
    assert_eq!(eval("AMIN(AA)", &aa), 1.0);
    assert_eq!(eval("IXMAX(AA)", &aa), 2.0);
    assert_eq!(eval("IXMIN(AA)", &aa), 3.0);
}
