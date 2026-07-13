//! R14-4 — the dynamic stores `@n := v` and `@@n := v`.
//!
//! C's `STORE_OPERATOR` has two shapes (`sCalcPostfix.c:515-567`,
//! `aCalcPostfix.c:483-...`). The one the port had was the static store: retract
//! the `FETCH_A`/`FETCH_AA` just emitted and park a `STORE_x` on the operator
//! stack. The one it did not have is the DYNAMIC store: search the operator
//! stack for the pending `A_FETCH` (`@`) or `A_SFETCH`/`A_AFETCH` (`@@`) and
//! rewrite that entry IN PLACE into `A_STORE` / `A_SSTORE` / `A_ASTORE`. Nothing
//! is retracted — the index expression stays in the postfix, the value follows
//! it, and the store pops both:
//!
//! ```c
//! case A_STORE:                                 /* sCalcPerform.c:897-906 */
//!     toDouble(ps);  ps1 = ps;  DEC(ps);        /* value */
//!     toDouble(ps);  i = myNINT(ps->d);  DEC(ps);   /* index */
//!     if (i >= numArgs || i < 0) printf(...); else parg[i] = ps1->d;
//! ```
//!
//! The port raised `CALC_ERR_BAD_ASSIGNMENT` at compile time for every one of
//! these. R13-5 ported the FETCH half; this is the store half, in all four
//! spellings (sCalc `@:=`/`@@:=`, aCalc `@:=`/`@@:=`).
//!
//! Boundaries, one case each: index 0 / in range / out of range / negative /
//! rounded; value scalar vs array; store-terminated program; the store's own
//! depth accounting.

use epics_base_rs::calc::{
    ArrayInputs, ArrayStackValue, CalcError, StackValue, StringInputs, acalc, acalc_compile, calc,
    compile, scalc, scalc_compile,
};

fn s(expr: &str) -> (StringInputs, StackValue) {
    let mut inputs = StringInputs::new();
    inputs.num_vars[0] = 1.0; // A
    inputs.num_vars[1] = 2.0; // B
    inputs.str_vars[0] = "aa".into();
    let top = scalc(expr, &mut inputs).unwrap();
    (inputs, top)
}

fn a(expr: &str) -> (ArrayInputs, ArrayStackValue) {
    let mut inputs = ArrayInputs::new(4);
    inputs.num_vars[1] = 2.0; // B
    inputs.arrays[1] = vec![1.0, 2.0, 3.0, 4.0]; // BB
    let top = acalc(expr, &mut inputs).unwrap();
    (inputs, top)
}

// ---- sCalc `@n := v` (A_STORE) ------------------------------------------

/// The index is the operand `@` was going to fetch: `@1` is B. The store must
/// write B and leave nothing on the stack — the `;1` is the program's value.
#[test]
fn scalc_dynamic_store_writes_the_indexed_scalar() {
    let (inputs, top) = s("@1:=5;1");
    assert_eq!(inputs.num_vars[1], 5.0);
    assert_eq!(top, StackValue::Double(1.0));
    // Index 0 is A — the boundary at the bottom of the range.
    let (inputs, _) = s("@0:=7;1");
    assert_eq!(inputs.num_vars[0], 7.0);
}

/// The index is an EXPRESSION, rounded with `myNINT` — `@(0.6)` is B, not A.
#[test]
fn scalc_dynamic_store_rounds_a_computed_index() {
    let (inputs, _) = s("@(0.6):=5;1");
    assert_eq!(inputs.num_vars[1], 5.0);
    assert_eq!(inputs.num_vars[0], 1.0, "A must not have been written");

    let (inputs, _) = s("@(A+1):=9;1"); // A is 1 -> index 2 = C
    assert_eq!(inputs.num_vars[2], 9.0);
}

/// Out of range — above the argument count and below zero — prints in C and
/// stores nothing. It is NOT an error: the expression still yields its value.
#[test]
fn scalc_dynamic_store_out_of_range_stores_nothing() {
    let (inputs, top) = s("@99:=5;1");
    assert_eq!(top, StackValue::Double(1.0));
    assert!(inputs.num_vars.iter().all(|&v| v != 5.0));

    let (inputs, top) = s("@(0-1):=5;1");
    assert_eq!(top, StackValue::Double(1.0));
    assert!(inputs.num_vars.iter().all(|&v| v != 5.0));
}

/// The store consumes index AND value and pushes nothing, so a program that is
/// only a store ends at depth 0 — CALC_ERR_INCOMPLETE, exactly like `A:=5`.
#[test]
fn a_dynamic_store_is_not_an_expression() {
    assert_eq!(scalc_compile("@1:=5").unwrap_err(), CalcError::Incomplete);
    assert_eq!(
        scalc_compile(r#"@@1:="x""#).unwrap_err(),
        CalcError::Incomplete
    );
    assert_eq!(acalc_compile("@1:=5").unwrap_err(), CalcError::Incomplete);
}

// ---- sCalc `@@n := v` (A_SSTORE) ----------------------------------------

/// `@@1` is BB, a STRING argument, and the value is coerced with `toString`.
#[test]
fn scalc_dynamic_string_store_writes_the_indexed_string() {
    let (inputs, _) = s(r#"@@1:="xyz";1"#);
    assert_eq!(inputs.str_vars[1].as_str_lossy(), "xyz");

    // A double value is converted, as STORE_AA does (cvtDoubleToString, prec 8).
    let (inputs, _) = s("@@0:=12;1");
    assert_eq!(inputs.str_vars[0].as_str_lossy(), "12.00000000");
}

/// `@@` is `A_SFETCH`, which IS in C's USES_STRING list — so a program with a
/// dynamic STRING store always runs the string evaluator, which is the only one
/// with a `case A_SSTORE`.
#[test]
fn a_dynamic_string_store_latches_uses_string() {
    assert!(scalc_compile(r#"@@1:="x";1"#).unwrap().uses_string);
    // The scalar `@` does not — and does not need to: A_STORE has a case on
    // BOTH of C's evaluator paths (sCalcPerform.c:440 and :897).
    assert!(!scalc_compile("@1:=5;1").unwrap().uses_string);
}

// ---- aCalc `@n := v` (A_STORE) and `@@n := v` (A_ASTORE) ----------------

/// aCalc has the same two stores; its `@@` names an ARRAY argument.
#[test]
fn acalc_dynamic_stores_write_scalar_and_array_args() {
    let (inputs, _) = a("@1:=5;1");
    assert_eq!(inputs.num_vars[1], 5.0);

    // `@@0` is AA: an array value is copied element-wise.
    let (inputs, top) = a("@@0:=BB*2;SUM(AA)");
    assert_eq!(inputs.arrays[0], vec![2.0, 4.0, 6.0, 8.0]);
    assert_eq!(top, ArrayStackValue::Double(20.0));
}

/// A SCALAR value is broadcast across the whole arraySize buffer, as STORE_AA
/// does (`aCalcPerform.c:519`), and the store marks the field in `amask` so the
/// record posts it.
#[test]
fn acalc_dynamic_array_store_broadcasts_and_marks_amask() {
    let (inputs, _) = a("@@0:=3;1");
    assert_eq!(inputs.arrays[0], vec![3.0, 3.0, 3.0, 3.0]);
    assert_eq!(inputs.amask & 1, 1, "AA's amask bit must be set");

    // The scalar store does NOT touch amask — it writes a scalar arg.
    let (inputs, _) = a("@1:=5;1");
    assert_eq!(inputs.amask, 0);
}

/// Out of range: no store, no amask bit, no error.
#[test]
fn acalc_dynamic_array_store_out_of_range_stores_nothing() {
    let (inputs, top) = a("@@99:=3;1");
    assert_eq!(top, ArrayStackValue::Double(1.0));
    assert_eq!(inputs.amask, 0);
    assert!(inputs.arrays[0].is_empty());
}

// ---- base ---------------------------------------------------------------

/// base's element table has no `@` at all (`postfix.c`), so the dynamic stores
/// are not "unsupported" there — they do not lex.
#[test]
fn base_has_no_dynamic_store() {
    assert_eq!(compile("@1:=5;1").unwrap_err(), CalcError::Syntax);
    let mut inputs = epics_base_rs::calc::NumericInputs::new();
    assert!(calc("A:=5;A", &mut inputs).is_ok()); // the static store still works
}
