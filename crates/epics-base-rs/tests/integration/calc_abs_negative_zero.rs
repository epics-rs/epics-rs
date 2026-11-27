//! R19-4 — `ABS` is `fabs` in base and a CONDITIONAL NEGATE in sCalc/aCalc.
//!
//! The three C dialects genuinely disagree, and the disagreement is visible on
//! exactly one value: negative zero.
//!
//! ```c
//! *ptop = fabs(*ptop);                          /* base   calcPerform.c:173-175 */
//! if (*pd < 0) *pd *= -1;                       /* sCalc  sCalcPerform.c:513-515 */
//! if (ps->d < 0) ps->d *= -1;                   /* sCalc  sCalcPerform.c:1046-1049 */
//! for (...) if (ps->a[i] < 0) ps->a[i] *= -1;   /* aCalc  aCalcPerform.c:771 */
//! if (ps->d < 0) {ps->d *= -1;}                 /* aCalc  aCalcPerform.c:1040 */
//! ```
//!
//! `-0.0 < 0` is FALSE, so the synApps engines leave a negative zero alone while
//! `fabs` clears its sign bit. Compiled on this host (all four `*Perform.c` linked
//! against libCom), `ABS(0*(0-1))`:
//!
//! ```text
//! base   ABS(0*(0-1)) = 0   signbit=0
//! sCalc  ABS(0*(0-1)) = -0  signbit=1
//! aCalc  ABS(0*(0-1)) = -0  signbit=1
//! aCalc  ABS(AA)  with AA=[-3,-0,-3,-0]  ->  [3,-0,3,-0]
//! ```
//!
//! The port used `f64::abs` in all three, so sCalc and aCalc answered `+0.0`.

use epics_base_rs::calc::{ArrayInputs, ArrayStackValue, NumericInputs, StringInputs, acalc, calc};
use epics_base_rs::calc::{StackValue, scalc};

/// The expression that produces a negative zero without a `-0` literal: `0 * -1`.
const NEG_ZERO: &str = "0*(0-1)";

#[test]
fn base_abs_is_fabs_and_clears_the_sign_of_negative_zero() {
    let v = calc(&format!("ABS({NEG_ZERO})"), &mut NumericInputs::new()).unwrap();
    assert_eq!(v, 0.0);
    assert!(
        v.is_sign_positive(),
        "base calls fabs (calcPerform.c:174), so the sign bit is cleared"
    );
    assert_eq!(calc("ABS(0-3)", &mut NumericInputs::new()).unwrap(), 3.0);
}

#[test]
fn scalc_abs_keeps_the_sign_of_negative_zero() {
    let v = match scalc(&format!("ABS({NEG_ZERO})"), &mut StringInputs::new()).unwrap() {
        StackValue::Double(v) => v,
        other => panic!("expected a Double, got {other:?}"),
    };
    assert_eq!(v, 0.0);
    assert!(
        v.is_sign_negative(),
        "`if (*pd < 0) *pd *= -1` does not fire on -0.0"
    );

    // The negative control: a strictly negative value IS negated.
    assert_eq!(
        scalc("ABS(0-3)", &mut StringInputs::new()).unwrap(),
        StackValue::Double(3.0)
    );
}

#[test]
fn acalc_abs_keeps_the_sign_of_negative_zero() {
    let v = match acalc(&format!("ABS({NEG_ZERO})"), &mut ArrayInputs::new(4)).unwrap() {
        ArrayStackValue::Double(v) => v,
        other => panic!("expected a Double, got {other:?}"),
    };
    assert_eq!(v, 0.0);
    assert!(
        v.is_sign_negative(),
        "aCalcPerform.c:1040, the scalar branch"
    );
}

/// aCalc's ARRAY branch (`aCalcPerform.c:771`) is the same conditional negate
/// applied element-wise, so a buffer mixing negatives and negative zeroes keeps
/// each element's answer independent.
#[test]
fn acalc_abs_is_element_wise_and_keeps_each_negative_zero() {
    let mut inp = ArrayInputs::new(4);
    inp.arrays[0] = vec![-3.0, -0.0, -3.0, -0.0];

    let out = match acalc("ABS(AA)", &mut inp).unwrap() {
        ArrayStackValue::Array(cell) => cell.buf().to_vec(),
        other => panic!("expected an Array, got {other:?}"),
    };

    assert_eq!(out, vec![3.0, 0.0, 3.0, 0.0]);
    let signs: Vec<bool> = out.iter().map(|v| v.is_sign_negative()).collect();
    assert_eq!(
        signs,
        vec![false, true, false, true],
        "compiled aCalc: ABS([-3,-0,-3,-0]) = [3,-0,3,-0]"
    );
}
