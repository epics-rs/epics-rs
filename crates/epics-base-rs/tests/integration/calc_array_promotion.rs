//! R9-2 — aCalc's scalar->array promotion is not a plain broadcast: C's
//! `to_array(..., setValues=1)` (`aCalcPerform.c:124-143`) fills the array with
//! **0** when the scalar is NaN (`:134-137`), and with the scalar otherwise.
//!
//! C's binary arms promote the LEFT operand only (`toArray(ps,1)` at `:625` and
//! `:1327`) and then read the right operand as the plain double `ps1->d`
//! (`:654-679`). So the two mixed shapes are deliberately NOT mirror images, and
//! the port — which broadcast the NaN verbatim in both — has to reproduce the
//! asymmetry, not smooth it over.
//!
//! Expectations are lines printed by `aCalcPostfix.c` + `aCalcPerform.c` built
//! standalone out of `epics-modules/calc/calcApp/src`. `ACOS(2)` is how a NaN
//! scalar is spelled: aCalc's element table has no `NAN` literal.

use epics_base_rs::calc::{ArrayInputs, ArrayStackValue, CalcError, acalc};

fn run(expr: &str) -> Result<ArrayStackValue, CalcError> {
    let mut inp = ArrayInputs::new(8);
    inp.arrays[0] = vec![1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0, 8.0];
    acalc(expr, &mut inp)
}

fn arr(expr: &str) -> Vec<f64> {
    match run(expr).expect("aCalcPerform returns st=0 here") {
        ArrayStackValue::Array(v) => v.into_buf(),
        ArrayStackValue::Double(d) => panic!("{expr}: expected an array, got {d}"),
    }
}

/// A NaN scalar on the LEFT is promoted, and the promotion zeroes it.
/// C: `ACOS(2)+AA` -> st=0, a=[1,2,3,4,5,6,7,8]  (0 + AA, not NaN + AA)
/// C: `ACOS(2)*AA` -> st=0, a=[0,0,0,0,0,0,0,0]  (0 * AA)
#[test]
fn nan_scalar_promoted_to_an_array_fills_zero() {
    assert_eq!(arr("ACOS(2)+AA"), vec![1., 2., 3., 4., 5., 6., 7., 8.]);
    assert_eq!(arr("ACOS(2)*AA"), vec![0.; 8]);
}

/// A NaN scalar on the RIGHT is NOT promoted — C reads it as `ps1->d`, so it
/// lands in every element.
/// C: `AA+ACOS(2)` -> a=[nan x8] (and st=-1, which `acalcoutRecord` turns into
/// CALC_ALARM after storing the values — `aCalcPerform.c:1620-1644` writes the
/// result before its non-finite tail returns -1).
#[test]
fn nan_scalar_on_the_right_is_not_promoted() {
    let v = arr("AA+ACOS(2)");
    assert_eq!(v.len(), 8);
    assert!(
        v.iter().all(|x| x.is_nan()),
        "C: a=[nan x8] for AA+ACOS(2) — the promotion rule must not rescue this \
         side into zeros; got {v:?}"
    );
}

/// A finite scalar promotes to itself, both ways round — the NaN rule is the
/// only special case in `to_array`.
/// C: `2+AA` -> [3,4,5,6,7,8,9,10]; `AA+2` -> [3,4,5,6,7,8,9,10].
#[test]
fn finite_scalar_promotion_is_a_plain_fill() {
    assert_eq!(arr("2+AA"), vec![3., 4., 5., 6., 7., 8., 9., 10.]);
    assert_eq!(arr("AA+2"), vec![3., 4., 5., 6., 7., 8., 9., 10.]);
}
