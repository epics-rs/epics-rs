//! aCalc's `**` / `^` (POWER) — the one binary operator that does NOT pair
//! element-wise.
//!
//! POWER is not a member of C's two-arg array dispatch. It is its own case
//! (`aCalcPerform.c:1306-1317`), and it collapses the EXPONENT with `toDouble(ps1)`
//! before looking at the left operand at all:
//!
//! ```c
//! case POWER:
//!     ps1 = ps; DEC(ps);
//!     toDouble(ps1);
//!     if (isArray(ps)) { for (i=0; i<arraySize; i++) ps->a[i] = pow(ps->a[i], ps1->d); }
//!     else             { ps->d = pow(ps->d, ps1->d); }
//! ```
//!
//! So an array exponent contributes only its a[0], and the LEFT operand alone
//! decides the result's shape. The port routed POWER through `zip_map`, which
//! promotes the left operand — so `2**AA` answered the array [2,4,8,16,32,64]
//! where C answers the scalar 2, and the result's VALUE and SHAPE both diverged.
//!
//! Every expected value below was read off the compiled upstream aCalc
//! (`aCalcPerform.c` + `aCalcPostfix.c` + `calcUtil.c`, gcc 13, arraySize 6).

use epics_base_rs::calc::{ArrayInputs, ArrayStackValue, acalc};

/// AA=[1,2,3,4,5,6], CC=[3,1,1,1,1,1]
fn inputs() -> ArrayInputs {
    let mut i = ArrayInputs::new(6);
    i.arrays[0] = vec![1.0, 2.0, 3.0, 4.0, 5.0, 6.0];
    i.arrays[2] = vec![3.0, 1.0, 1.0, 1.0, 1.0, 1.0];
    i
}

fn eval(expr: &str) -> ArrayStackValue {
    let mut i = inputs();
    acalc(expr, &mut i).unwrap()
}

fn arr(expr: &str) -> Vec<f64> {
    match eval(expr) {
        ArrayStackValue::Array(c) => c.buf().to_vec(),
        ArrayStackValue::Double(d) => panic!("expected an ARRAY result, got the scalar {d}"),
    }
}

fn d(expr: &str) -> f64 {
    match eval(expr) {
        ArrayStackValue::Double(v) => v,
        ArrayStackValue::Array(c) => {
            panic!("expected a SCALAR result, got the array {:?}", c.buf())
        }
    }
}

#[test]
fn scalar_base_array_exponent_is_a_scalar() {
    // THE finding. compiled C: `2**AA` is the scalar 2 — pow(2, AA[0]) = pow(2,1).
    // The port answered the array [2,4,8,16,32,64].
    assert_eq!(d("2**AA"), 2.0);
}

#[test]
fn array_base_scalar_exponent_is_elementwise() {
    // compiled C: `AA**2` -> [1,4,9,16,25,36]
    assert_eq!(arr("AA**2"), vec![1.0, 4.0, 9.0, 16.0, 25.0, 36.0]);
}

#[test]
fn array_exponent_contributes_only_its_first_element() {
    // compiled C: `AA**CC` with CC=[3,1,1,1,1,1] -> [1,8,27,64,125,216], i.e. AA**3.
    // CC[1..] is never read. The port paired element-wise and answered
    // [1,2,3,4,5,6].
    assert_eq!(arr("AA**CC"), vec![1.0, 8.0, 27.0, 64.0, 125.0, 216.0]);
}

#[test]
fn scalar_base_scalar_exponent_stays_scalar() {
    // compiled C: `2**3` -> 8
    assert_eq!(d("2**3"), 8.0);
}

#[test]
fn caret_is_power_not_xor() {
    // aCalcPostfix.c:217,237 — both `**` and `^` compile to POWER.
    assert_eq!(arr("AA^2"), vec![1.0, 4.0, 9.0, 16.0, 25.0, 36.0]);
    assert_eq!(d("2^AA"), 2.0);
}

#[test]
fn power_keeps_the_left_operands_window() {
    // C writes ps->a[i] in place and never touches ps->numEl, so the left operand's
    // window survives. compiled C, AA=[1,5,2,8,3,9]:
    //   (AA[1,3])**2      -> [25,4,64,0,0,0]
    //   AVG((AA[1,3])**2) -> 31   (the 3-element window's own average)
    let mut i = ArrayInputs::new(6);
    i.arrays[0] = vec![1.0, 5.0, 2.0, 8.0, 3.0, 9.0];
    let got = match acalc("(AA[1,3])**2", &mut i).unwrap() {
        ArrayStackValue::Array(c) => c.buf().to_vec(),
        other => panic!("expected an array, got {other:?}"),
    };
    assert_eq!(got, vec![25.0, 4.0, 64.0, 0.0, 0.0, 0.0]);

    let mut i = ArrayInputs::new(6);
    i.arrays[0] = vec![1.0, 5.0, 2.0, 8.0, 3.0, 9.0];
    match acalc("AVG((AA[1,3])**2)", &mut i).unwrap() {
        ArrayStackValue::Double(v) => assert_eq!(v, 31.0),
        other => panic!("expected a scalar, got {other:?}"),
    }
}
