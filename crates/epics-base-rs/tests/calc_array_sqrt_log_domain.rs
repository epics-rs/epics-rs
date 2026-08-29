//! aCalc's SQRT/LOG/LN domain guard has two branches that deliberately DISAGREE
//! (`aCalcPerform.c:775-812` array, `:1044-1072` scalar), and the port produced a
//! bare NaN in both:
//!
//! - scalar: a negative operand becomes 0, **no error** — C only prints a line.
//! - array:  negative elements become 0 **and** `status` = -1, so aCalcPerform
//!   returns -1 (`:1591`) without writing p_dresult/p_aresult.
//!
//! Ground truth is the compiled synApps `aCalcPerform` (arraySize 8).

use epics_base_rs::calc::{ArrayInputs, ArrayStackValue, CalcError, acalc};

fn scalar(expr: &str, a: f64) -> Result<f64, CalcError> {
    let mut inputs = ArrayInputs::new(8);
    inputs.num_vars[0] = a;
    acalc(expr, &mut inputs).map(|r| match r {
        ArrayStackValue::Double(v) => v,
        other => panic!("{expr} on a scalar must stay a Double, got {other:?}"),
    })
}

fn array(expr: &str, aa: &[f64]) -> Result<ArrayStackValue, CalcError> {
    let mut inputs = ArrayInputs::new(8);
    inputs.arrays[0] = aa.to_vec();
    acalc(expr, &mut inputs)
}

/// C: `SQRT(-4)` = 0 with status 0 — a healthy record, not an alarm and not NaN.
/// The port returned NaN, which acalcout's non-finite check turned into a
/// CALC_ALARM C never raises.
#[test]
fn a_negative_scalar_is_zero_and_no_error() {
    assert_eq!(scalar("SQRT(A)", -4.0).unwrap(), 0.0);
    assert_eq!(scalar("LOG(A)", -4.0).unwrap(), 0.0);
    assert_eq!(scalar("LN(A)", -4.0).unwrap(), 0.0);
}

/// The non-negative scalar path is untouched.
#[test]
fn a_non_negative_scalar_still_computes() {
    assert_eq!(scalar("SQRT(A)", 9.0).unwrap(), 3.0);
    assert_eq!(scalar("LOG(A)", 100.0).unwrap(), 2.0);
}

/// C: the guard is `< 0`, so LOG(0) is NOT caught — it yields -inf, and the
/// record's own non-finite check owns that outcome.
#[test]
fn log_of_zero_is_not_caught_by_the_guard() {
    assert!(scalar("LOG(A)", 0.0).unwrap().is_infinite());
}

/// C: a negative ELEMENT sets status = -1, so aCalcPerform returns -1 and writes
/// no result. The port yielded an array with a NaN in it and a healthy status —
/// and because acalcout only checks a[0], an off-index negative missed the alarm
/// entirely. The negative here is at index 1, not 0, which is exactly that case.
#[test]
fn a_negative_element_fails_the_whole_evaluation() {
    for expr in ["SQRT(AA)", "LOG(AA)", "LN(AA)"] {
        assert_eq!(
            array(expr, &[4.0, -9.0, 16.0, 1.0, 1.0, 1.0, 1.0, 1.0]),
            Err(CalcError::DomainError),
            "{expr} with a negative element must fail the evaluation"
        );
    }
}

/// An all-non-negative array is unaffected.
#[test]
fn a_non_negative_array_still_computes() {
    assert_eq!(
        array("SQRT(AA)", &[4.0, 9.0, 16.0, 25.0, 0.0, 1.0, 1.0, 1.0]).unwrap(),
        ArrayStackValue::array(vec![2.0, 3.0, 4.0, 5.0, 0.0, 1.0, 1.0, 1.0])
    );
}

/// C's `status` is a DEFERRED flag, not an early return: execution continues to
/// the end of the expression, so a store sequenced AFTER the failing operator
/// still lands in the record's fields even though the evaluation reports -1.
///
/// Compiled aCalcPerform, `BB:=SQRT(AA);C:=7;1` with AA=[4,-9,16,1,1,1,1,1]:
///   status=-1, amask=2, BB=[2,0,4,1,1,1,1,1], C=7
#[test]
fn a_deferred_failure_still_lets_later_stores_land() {
    let mut inputs = ArrayInputs::new(8);
    inputs.arrays[0] = vec![4.0, -9.0, 16.0, 1.0, 1.0, 1.0, 1.0, 1.0];

    let result = acalc("BB:=SQRT(AA);C:=7;1", &mut inputs);

    assert_eq!(
        result,
        Err(CalcError::DomainError),
        "the evaluation reports -1"
    );
    assert_eq!(
        inputs.arrays[1],
        vec![2.0, 0.0, 4.0, 1.0, 1.0, 1.0, 1.0, 1.0],
        "BB's store landed before the deferred failure was consumed"
    );
    assert_eq!(
        inputs.num_vars[2], 7.0,
        "C's store, sequenced AFTER the failing SQRT, also landed"
    );
}
