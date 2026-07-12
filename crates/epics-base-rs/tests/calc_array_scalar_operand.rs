//! aCalc's unary array operators have a SCALAR branch (`aCalcPerform.c:1036-1101`)
//! — C answers every one of them for a scalar operand, it never raises a type
//! error. The port refused all of them with TypeMismatch, which a record turns
//! into CALC_ALARM/INVALID and a frozen VAL, so legal expressions like `AVG(5)`
//! were unusable.
//!
//! Every expectation is the compiled synApps `aCalcPerform` run as `<OP>(A)`
//! with `A=5` (arraySize 8), reading back `dresult`.

use epics_base_rs::calc::{ArrayInputs, ArrayStackValue, acalc};

fn scalar_op(expr: &str, a: f64) -> f64 {
    let mut inputs = ArrayInputs::new(8);
    inputs.num_vars[0] = a;
    match acalc(expr, &mut inputs).unwrap() {
        ArrayStackValue::Double(v) => v,
        other => panic!("{expr} on a scalar must stay a Double, got {other:?}"),
    }
}

/// C `case AVERAGE/ARRSUM/CUM/SMOOTH/AMAX/AMIN: break;` — the scalar branch does
/// not touch `ps->d`, so the operand passes straight through.
#[test]
fn pass_through_operators_return_the_scalar() {
    for expr in [
        "AVG(A)", "SUM(A)", "CUM(A)", "SMOO(A)", "AMAX(A)", "AMIN(A)",
    ] {
        assert_eq!(scalar_op(expr, 5.0), 5.0, "{expr} must pass 5 through");
    }
}

/// C `case STD_DEV/FWHM/DERIV: ps->d = 0;` and `case IXMAX/IXMIN: ps->d = 0;` —
/// a single point has no spread, no width, no slope, and its own index is 0.
#[test]
fn zeroing_operators_return_zero() {
    for expr in ["STD(A)", "FWHM(A)", "DERIV(A)", "IXMAX(A)", "IXMIN(A)"] {
        assert_eq!(scalar_op(expr, 5.0), 0.0, "{expr} must answer 0");
    }
}

/// C `case IXZ: ps->d = fabs(ps->d)<SMALL ? 0 : -1;` — a scalar is its own
/// element 0, so it "contains a zero" exactly when it is one.
#[test]
fn ixz_on_a_scalar_thresholds_at_small() {
    assert_eq!(scalar_op("IXZ(A)", 0.0), 0.0);
    assert_eq!(scalar_op("IXZ(A)", 1e-12), 0.0);
    assert_eq!(scalar_op("IXZ(A)", 5.0), -1.0);
}

/// C `case IXNZ: ps->d = fabs(ps->d)>SMALL ? 0 : -1;` — the mirror image.
#[test]
fn ixnz_on_a_scalar_thresholds_at_small() {
    assert_eq!(scalar_op("IXNZ(A)", 5.0), 0.0);
    assert_eq!(scalar_op("IXNZ(A)", 0.0), -1.0);
    assert_eq!(scalar_op("IXNZ(A)", 1e-12), -1.0);
}

/// The whole point of the finding: these expressions must EVALUATE, not error.
/// A TypeMismatch here is what the record turns into CALC_ALARM/INVALID.
#[test]
fn no_unary_array_operator_errors_on_a_scalar() {
    let mut inputs = ArrayInputs::new(8);
    inputs.num_vars[0] = 5.0;
    for expr in [
        "AVG(A)", "STD(A)", "FWHM(A)", "SUM(A)", "AMAX(A)", "AMIN(A)", "IXMAX(A)", "IXMIN(A)",
        "IXZ(A)", "IXNZ(A)", "SMOO(A)", "DERIV(A)", "CUM(A)",
    ] {
        assert!(
            acalc(expr, &mut inputs).is_ok(),
            "{expr} is legal in C and must not raise a calc error"
        );
    }
}

/// The array branch must be untouched by the scalar branch's arrival.
#[test]
fn the_array_branch_still_answers() {
    let mut inputs = ArrayInputs::new(8);
    inputs.arrays[0] = vec![1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0, 8.0];
    match acalc("AVG(AA)", &mut inputs).unwrap() {
        ArrayStackValue::Double(v) => assert_eq!(v, 4.5),
        other => panic!("AVG over an array reduces to a Double, got {other:?}"),
    }
    match acalc("SUM(AA)", &mut inputs).unwrap() {
        ArrayStackValue::Double(v) => assert_eq!(v, 36.0),
        other => panic!("SUM over an array reduces to a Double, got {other:?}"),
    }
}
