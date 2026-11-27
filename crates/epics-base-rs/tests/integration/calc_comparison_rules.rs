//! R9-1 — each engine's comparison rule, and they are not the same rule.
//!
//! * base `calcPerform.c:368-392` — the bare C operators, exact.
//! * aCalc `aCalcPerform.c:1345-1350` (array/array), `:1370-1375`
//!   (array/scalar), `:1386-1391` (scalar/scalar) — also the bare C operators,
//!   exact. `aCalcPerform` contains no epsilon at all.
//! * sCalc `sCalcPerform.c:595-620` (no-string path) and `:1161-1255` (string
//!   path) — every numeric comparison is written around `SMALL` = 1e-11
//!   (`:45`): `a == b` is `fabs(a-b) < SMALL`, `a > b` is `(a-b) > SMALL`, and
//!   so on. Two strings go through `strcmp`.
//!
//! The port had these two SWAPPED: the array engine applied a 1e-11 epsilon
//! (which is sCalc's constant, in the engine that has none) and the string
//! engine compared exactly (base's rule, in the engine that has the epsilon).
//!
//! Every expectation is a line printed by the C evaluators built standalone out
//! of `epics-modules/calc/calcApp/src` (base's from `postfix.c`+`calcPerform.c`).

use epics_base_rs::calc::{
    ArrayInputs, ArrayStackValue, NumericInputs, StackValue, StringInputs, acalc, calc, scalc,
};

fn base(expr: &str) -> f64 {
    let mut inp = NumericInputs::new();
    calc(expr, &mut inp).unwrap()
}

fn sc(expr: &str) -> f64 {
    let mut inp = StringInputs::new();
    match scalc(expr, &mut inp).unwrap() {
        StackValue::Double(v) => v,
        StackValue::Str(s) => panic!("expected a double, got {s:?}"),
    }
}

fn ac(expr: &str) -> f64 {
    let mut inp = ArrayInputs::new(8);
    match acalc(expr, &mut inp).unwrap() {
        ArrayStackValue::Double(v) => v,
        ArrayStackValue::Array(v) => panic!("expected a double, got {v:?}"),
    }
}

fn ac_arr(expr: &str) -> Vec<f64> {
    let mut inp = ArrayInputs::new(8);
    inp.arrays[0] = vec![1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0, 8.0];
    match acalc(expr, &mut inp).unwrap() {
        ArrayStackValue::Array(v) => v.into_buf(),
        ArrayStackValue::Double(d) => panic!("expected an array, got {d}"),
    }
}

/// aCalc is EXACT. C: `1e-12 == 0` -> 0, `1e-12 # 0` -> 1, `1e-12 < 1e-11` -> 1,
/// `1e-12 <= 0` -> 0, `0.1+0.2 # 0.3` -> 1.
#[test]
fn acalc_compares_exactly() {
    assert_eq!(ac("1e-12 == 0"), 0.0);
    assert_eq!(ac("1e-12 # 0"), 1.0);
    assert_eq!(ac("1e-12 < 1e-11"), 1.0);
    assert_eq!(ac("1e-12 <= 0"), 0.0);
    // The classic ULP case: exactly what an epsilon would have hidden.
    assert_eq!(ac("0.1+0.2 # 0.3"), 1.0);
    assert_eq!(ac("1 >= 1"), 1.0);
}

/// The array shapes take the same exact operators, element by element.
/// C (AA=[1..8]): `AA == 2` -> [0,1,0,0,0,0,0,0]; `AA > 2` -> [0,0,1,1,1,1,1,1];
/// `AA >= 2` -> [0,1,1,1,1,1,1,1]; `AA # 2` -> [1,0,1,1,1,1,1,1].
#[test]
fn acalc_array_comparisons_are_exact_element_wise() {
    assert_eq!(ac_arr("AA == 2"), vec![0., 1., 0., 0., 0., 0., 0., 0.]);
    assert_eq!(ac_arr("AA > 2"), vec![0., 0., 1., 1., 1., 1., 1., 1.]);
    assert_eq!(ac_arr("AA >= 2"), vec![0., 1., 1., 1., 1., 1., 1., 1.]);
    assert_eq!(ac_arr("AA # 2"), vec![1., 0., 1., 1., 1., 1., 1., 1.]);
}

/// sCalc compares within `SMALL` = 1e-11. C: `1e-12 == 0` -> 1, `1e-12 # 0` -> 0,
/// `1e-12 < 1e-11` -> 0 (the 9e-12 gap does not exceed SMALL), `1e-12 <= 0` -> 1,
/// `1e-12 >= 0` -> 1, `0.1+0.2 == 0.3` -> 1.
#[test]
fn scalc_compares_within_small() {
    assert_eq!(sc("1e-12 == 0"), 1.0);
    assert_eq!(sc("1e-12 # 0"), 0.0);
    assert_eq!(sc("1e-12 < 1e-11"), 0.0);
    assert_eq!(sc("1e-12 <= 0"), 1.0);
    assert_eq!(sc("1e-12 >= 0"), 1.0);
    assert_eq!(sc("0.1+0.2 == 0.3"), 1.0);
    // Differences well beyond SMALL still answer normally.
    assert_eq!(sc("2 > 1"), 1.0);
    assert_eq!(sc("1 > 2"), 0.0);
    assert_eq!(sc("1 >= 1"), 1.0);
}

/// Two strings are `strcmp`'d (`sCalcPerform.c:1167-1169`) — the epsilon is a
/// numeric rule only.
#[test]
fn scalc_string_comparison_is_strcmp() {
    assert_eq!(sc(r#""abc" == "abc""#), 1.0);
    assert_eq!(sc(r#""abc" == "abd""#), 0.0);
    assert_eq!(sc(r#""abc" < "abd""#), 1.0);
}

/// base has no epsilon (`calcPerform.c:368-392`) — the numeric engine must not
/// pick up sCalc's.
#[test]
fn base_compares_exactly() {
    assert_eq!(base("1e-12 == 0"), 0.0);
    assert_eq!(base("1e-12 # 0"), 1.0);
    assert_eq!(base("1e-12 < 1e-11"), 1.0);
    assert_eq!(base("0.1+0.2 # 0.3"), 1.0);
}
