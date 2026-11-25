//! aCalc's three float predicates have two different shapes, and the port had
//! collapsed both to `a[0]`:
//!
//! - `ISINF` is an element-wise UNARY operator (`aCalcPerform.c:826`, :1085) —
//!   an array operand yields an ARRAY result.
//! - `FINITE` / `ISNAN` are VARARG reductions over EVERY element of EVERY
//!   argument (`:1103-1109`, `:1127-1135`) — always a scalar result.
//!
//! Expectations are the compiled synApps `aCalcPerform` (arraySize 8).

use epics_base_rs::calc::{ArrayInputs, ArrayStackValue, acalc};

fn eval(expr: &str, aa: &[f64], bb: &[f64], a: f64) -> ArrayStackValue {
    let mut inputs = ArrayInputs::new(8);
    inputs.arrays[0] = aa.to_vec();
    inputs.arrays[1] = bb.to_vec();
    inputs.num_vars[0] = a;
    acalc(expr, &mut inputs).unwrap()
}

fn scalar(expr: &str, aa: &[f64], bb: &[f64], a: f64) -> f64 {
    match eval(expr, aa, bb, a) {
        ArrayStackValue::Double(v) => v,
        other => panic!("{expr} must reduce to a Double, got {other:?}"),
    }
}

const FINITE_8: [f64; 8] = [1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0, 8.0];

fn with_inf_at_2() -> [f64; 8] {
    [1.0, 2.0, f64::INFINITY, 4.0, 5.0, 6.0, 7.0, 8.0]
}

fn with_nan_at_2() -> [f64; 8] {
    [1.0, 2.0, f64::NAN, 4.0, 5.0, 6.0, 7.0, 8.0]
}

/// C: `ISINF([1,2,inf,4..])` is the ARRAY [0,0,1,0,0,0,0,0]. The port collapsed
/// it to the scalar `isinf(a[0])` = 0 — the wrong shape entirely, so an off-index
/// infinity was invisible.
#[test]
fn isinf_on_an_array_is_element_wise() {
    assert_eq!(
        eval("ISINF(AA)", &with_inf_at_2(), &[], 0.0),
        ArrayStackValue::array(vec![0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 0.0])
    );
}

/// C: a scalar operand keeps ISINF scalar.
#[test]
fn isinf_on_a_scalar_stays_scalar() {
    assert_eq!(scalar("ISINF(A)", &[], &[], f64::INFINITY), 1.0);
    assert_eq!(scalar("ISINF(A)", &[], &[], 5.0), 0.0);
}

/// C: `FINITE([1,2,inf,4..])` = 0 — the AND runs over ALL elements, so an
/// infinity anywhere in the array defeats it. The port answered `finite(a[0])`
/// = 1.
#[test]
fn finite_ands_over_every_element() {
    assert_eq!(scalar("FINITE(AA)", &with_inf_at_2(), &[], 0.0), 0.0);
    assert_eq!(scalar("FINITE(AA)", &FINITE_8, &[], 0.0), 1.0);
}

/// C: a NaN is not finite either, so FINITE also catches it.
#[test]
fn finite_is_defeated_by_a_nan_element() {
    assert_eq!(scalar("FINITE(AA)", &with_nan_at_2(), &[], 0.0), 0.0);
}

/// C: `ISNAN([1,2,nan,4..])` = 1 — the OR runs over ALL elements. The port
/// answered `isnan(a[0])` = 0.
#[test]
fn isnan_ors_over_every_element() {
    assert_eq!(scalar("ISNAN(AA)", &with_nan_at_2(), &[], 0.0), 1.0);
    assert_eq!(scalar("ISNAN(AA)", &FINITE_8, &[], 0.0), 0.0);
}

/// C: the reduction spans every ARGUMENT too, mixing shapes freely —
/// `FINITE(AA,BB)` with a clean AA and an inf inside BB is 0, and `ISNAN(A,BB)`
/// with a clean scalar A and a NaN inside BB is 1.
#[test]
fn the_reduction_spans_every_argument() {
    assert_eq!(
        scalar("FINITE(AA,BB)", &FINITE_8, &with_inf_at_2(), 0.0),
        0.0
    );
    assert_eq!(scalar("ISNAN(A,BB)", &[], &with_nan_at_2(), 0.0), 1.0);
    // ...and stays 1/0 when nothing anywhere is bad.
    assert_eq!(scalar("FINITE(AA,BB)", &FINITE_8, &FINITE_8, 1.0), 1.0);
    assert_eq!(scalar("ISNAN(A,BB)", &[], &FINITE_8, 1.0), 0.0);
}
