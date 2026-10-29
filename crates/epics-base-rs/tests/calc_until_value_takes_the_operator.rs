//! CBUG-AL1 — an operator after `UNTIL(...)` applies to the loop's value, as
//! it does after `(UNTIL(...))`. C pushes UNTIL_END at UNTIL's in_stack_pri 0
//! (`sCalcPostfix.c:779-783`, `aCalcPostfix.c:723-727`), so no operator pops
//! it and the operator compiles inside the loop: compiled C gives
//! `L:=UNTIL(K:=K+1;K>3)+10;K` = 1 with K=1, L=10. Not copied.
//!
//! Every other expected value is an output of the compiled upstream sCalc and
//! aCalc (calc f207871) with all arguments 0. No operator follows the UNTIL in
//! them, so the port's order is C's.

use epics_base_rs::calc::{
    ArrayInputs, ArrayStackValue, CalcError, StackValue, StringInputs, acalc, acalc_compile, scalc,
    scalc_compile,
};

const K: usize = 10;
const L: usize = 11;

/// Both engines: the value, K and L.
fn both(e: &str, value: f64, k: f64, l: f64) {
    let mut s = StringInputs::new();
    assert_eq!(scalc(e, &mut s), Ok(StackValue::Double(value)), "s {e}");
    assert_eq!((s.num_vars[K], s.num_vars[L]), (k, l), "s {e}: K, L");
    let mut a = ArrayInputs::new(8);
    assert_eq!(
        acalc(e, &mut a),
        Ok(ArrayStackValue::Double(value)),
        "a {e}"
    );
    assert_eq!((a.num_vars[K], a.num_vars[L]), (k, l), "a {e}: K, L");
}

/// The values compiled C gives for the parenthesized form.
#[test]
fn an_operator_after_until_applies_to_its_value() {
    both("L:=UNTIL(K:=K+1;K>3)+10;K", 4.0, 4.0, 11.0);
    both("UNTIL(K:=K+1;K>3)+10", 11.0, 4.0, 0.0);
    both("UNTIL(K:=K+1;K>3)?7:8", 7.0, 4.0, 0.0);
    for (e, paren) in [
        ("UNTIL(1)+UNTIL(1)", "(UNTIL(1))+(UNTIL(1))"),
        ("UNTIL(K:=K+1;K>3)?7:8", "(UNTIL(K:=K+1;K>3))?7:8"),
    ] {
        let code = |p: Result<epics_base_rs::calc::CompiledExpr, _>| p.map(|p| p.code().to_vec());
        assert_eq!(code(scalc_compile(e)), code(scalc_compile(paren)), "s {e}");
        assert_eq!(code(acalc_compile(e)), code(acalc_compile(paren)), "a {e}");
    }
}

#[test]
fn with_no_operator_after_it_until_is_cs() {
    both("L:=UNTIL(K:=K+1;K>3);K", 4.0, 4.0, 1.0);
    both("L:=UNTIL (K:=K+1;K>3);K", 4.0, 4.0, 1.0);
    both("(UNTIL(K:=K+1;K>3))+10", 11.0, 4.0, 0.0);
    both("L:=MAX(UNTIL(K:=K+1;K>3),2);K", 4.0, 4.0, 2.0);
    both("L:=UNTIL(J:=J+1;I:=UNTIL(K:=K+1;K>J);J>2);K", 4.0, 4.0, 1.0);
    // A `(` after `?` or `:` is not UNTIL's: the loop still ends at `;`.
    both("L:=UNTIL 1?(K:=K+1;K>3):0;K", 4.0, 4.0, 1.0);
    both("L:=UNTIL K>9?1:(K:=K+1;K>3);K", 4.0, 4.0, 1.0);
    both("L:=UNTIL K>3;K", 0.0, 0.0, 0.0);
    for e in ["UNTIL(K:=K+1;K>3);K", "L:=UNTIL(K:=K+1;K>3)"] {
        assert_eq!(scalc_compile(e).err(), Some(CalcError::Incomplete), "s {e}");
        assert_eq!(acalc_compile(e).err(), Some(CalcError::Incomplete), "a {e}");
    }
}

/// The `)` that ends the loop pops its condition, so a condition that is an
/// assignment is refused (CBUG-AJ1): C's UNTIL_END takes the 3 below it. C
/// refuses the last itself.
#[test]
fn an_assignment_is_not_the_condition() {
    for (e, err) in [
        ("3;UNTIL(K:=K+1)", CalcError::Underflow),
        ("3;UNTIL(K:=K+1)+1", CalcError::Underflow),
        ("UNTIL(K:=K+1)+1", CalcError::Incomplete),
    ] {
        assert_eq!(scalc_compile(e).err(), Some(err.clone()), "s {e}");
        assert_eq!(acalc_compile(e).err(), Some(err), "a {e}");
    }
}
