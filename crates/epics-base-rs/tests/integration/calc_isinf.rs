//! CBUG-A3 — ISINF is a BOOLEAN predicate in every engine.
//!
//! DEVIATION from C, deliberate. C assigns the `isinf` macro's result straight
//! into a double (`calcPerform.c:276`, `sCalcPerform.c:703,1407`,
//! `aCalcPerform.c:826,1084`), and glibc's `isinf` is `__builtin_isinf_sign` —
//! it returns the SIGN, so `-inf` gives **-1** (verified by compiling the macro
//! on this host: isinf(+inf)=1, isinf(-inf)=-1, isinf(nan)=0, isinf(3)=0).
//! `calcRecord.dbd.pod:263` documents `ISINF (arg)` as "returns non-zero if any
//! argument is Inf", a predicate, and an IOC whose `isinf` resolves to the C99
//! macro answers +1 for -Inf — so C's own value depends on its libc. The port
//! pushes the documented boolean.
//!
//! This whole file used to PIN the -1 (it was named after the sign, and asserted
//! `isinf_of_negative_infinity_is_minus_one`).

use epics_base_rs::calc::{
    ArrayInputs, ArrayStackValue, NumericInputs, StackValue, StringInputs, acalc, calc, scalc,
};

/// The boundary C gets wrong: the negative infinity. C: -1.
#[test]
fn isinf_of_negative_infinity_is_one() {
    let mut inp = NumericInputs::new();
    inp.vars[0] = f64::NEG_INFINITY;
    assert_eq!(calc("ISINF(A)", &mut inp).unwrap(), 1.0);

    let mut sinp = StringInputs::new();
    sinp.num_vars[0] = f64::NEG_INFINITY;
    assert_eq!(
        scalc("ISINF(A)", &mut sinp).unwrap(),
        StackValue::Double(1.0)
    );

    let mut ainp = ArrayInputs::new(1);
    ainp.num_vars[0] = f64::NEG_INFINITY;
    assert_eq!(
        acalc("ISINF(A)", &mut ainp).unwrap(),
        ArrayStackValue::Double(1.0)
    );
}

/// The documented predicate, and the reason -1 was never usable: `ISINF(A) == 1`
/// is the natural test, and it has to hold for BOTH infinities.
#[test]
fn isinf_equals_one_is_true_for_either_infinity() {
    let mut inp = NumericInputs::new();
    for v in [f64::INFINITY, f64::NEG_INFINITY] {
        inp.vars[0] = v;
        assert_eq!(
            calc("ISINF(A)==1", &mut inp).unwrap(),
            1.0,
            "ISINF({v})==1 must hold"
        );
    }
}

/// The other boundaries are unchanged, in every engine: +Inf is 1, and finite
/// and NaN are 0.
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
/// predicate has to hold per element, not just in the scalar branch. C signs
/// each element, so element 0 (-inf) was -1 here.
#[test]
fn acalc_isinf_flags_every_element() {
    let mut inp = ArrayInputs::new(4);
    inp.arrays[0] = vec![f64::NEG_INFINITY, f64::INFINITY, 2.5, f64::NAN];
    match acalc("ISINF(AA)", &mut inp).unwrap() {
        ArrayStackValue::Array(a) => assert_eq!(a.buf()[..4], [1.0, 1.0, 0.0, 0.0]),
        ArrayStackValue::Double(d) => panic!("expected an array, got {d}"),
    }
}
