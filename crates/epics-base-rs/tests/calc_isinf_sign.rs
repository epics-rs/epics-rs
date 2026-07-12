//! R10-7 — C's ISINF pushes `isinf(x)` straight into a double
//! (`calcPerform.c:277`, `sCalcPerform.c:1407`, `aCalcPerform.c:826,1084`), and
//! glibc's `isinf` is `__builtin_isinf_sign`: it returns the SIGN, so `-inf`
//! gives -1, not 1. All three ports pushed a boolean.
//!
//! Compiled on this host: isinf(+inf)=1, isinf(-inf)=-1, isinf(nan)=0,
//! isinf(3)=0.

use epics_base_rs::calc::{
    ArrayInputs, ArrayStackValue, NumericInputs, StackValue, StringInputs, acalc, calc, scalc,
};

/// The boundary that was wrong: the negative infinity.
#[test]
fn isinf_of_negative_infinity_is_minus_one() {
    let mut inp = NumericInputs::new();
    inp.vars[0] = f64::NEG_INFINITY;
    assert_eq!(calc("ISINF(A)", &mut inp).unwrap(), -1.0);

    let mut sinp = StringInputs::new();
    sinp.num_vars[0] = f64::NEG_INFINITY;
    assert_eq!(
        scalc("ISINF(A)", &mut sinp).unwrap(),
        StackValue::Double(-1.0)
    );

    let mut ainp = ArrayInputs::new(1);
    ainp.num_vars[0] = f64::NEG_INFINITY;
    assert_eq!(
        acalc("ISINF(A)", &mut ainp).unwrap(),
        ArrayStackValue::Double(-1.0)
    );
}

/// Negative control — the other three boundaries of the sign function are
/// unchanged, in every engine.
#[test]
fn isinf_is_one_for_positive_infinity_and_zero_otherwise() {
    let mut inp = NumericInputs::new();
    for (v, want) in [
        (f64::INFINITY, 1.0),
        (f64::NAN, 0.0),
        (3.0, 0.0),
        (-0.0, 0.0),
    ] {
        inp.vars[0] = v;
        assert_eq!(calc("ISINF(A)", &mut inp).unwrap(), want, "calc ISINF({v})");

        let mut sinp = StringInputs::new();
        sinp.num_vars[0] = v;
        assert_eq!(
            scalc("ISINF(A)", &mut sinp).unwrap(),
            StackValue::Double(want),
            "scalc ISINF({v})"
        );

        let mut ainp = ArrayInputs::new(1);
        ainp.num_vars[0] = v;
        assert_eq!(
            acalc("ISINF(A)", &mut ainp).unwrap(),
            ArrayStackValue::Double(want),
            "acalc ISINF({v})"
        );
    }
}

/// aCalc's ISINF over an ARRAY is element-wise (`aCalcPerform.c:826`), so the
/// sign has to survive per element, not just in the scalar branch.
#[test]
fn acalc_isinf_signs_every_element() {
    let mut inp = ArrayInputs::new(4);
    inp.arrays[0] = vec![f64::NEG_INFINITY, f64::INFINITY, 2.5, f64::NAN];
    match acalc("ISINF(AA)", &mut inp).unwrap() {
        ArrayStackValue::Array(a) => assert_eq!(a[..4], [-1.0, 1.0, 0.0, 0.0]),
        ArrayStackValue::Double(d) => panic!("expected an array, got {d}"),
    }
}
