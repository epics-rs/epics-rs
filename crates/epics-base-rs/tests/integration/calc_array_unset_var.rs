//! An aCalc array variable the caller never set is an `arraySize` buffer of ZEROS,
//! not the scalar 0.
//!
//! C's `FETCH_AA..FETCH_LL` (`aCalcPerform.c:440-454`) makes the cell an array
//! before it reads anything into it:
//!
//! ```c
//! INC(ps); toArray(ps,0); ps->a[0] = 0.;
//! if (num_aArgs > i) {
//!     if (pp_aArg[i]) { for (i=0;i<arraySize;i++) ps->a[i] = pp_aArg[i][i]; }
//!     else            { for (i=0;i<arraySize;i++) ps->a[i] = 0.0; }   /* NULL field */
//! }
//! ```
//!
//! The port keyed off an empty `Vec` and pushed a scalar 0, which sent every
//! shape-sensitive operator above it into the WRONG BRANCH.
//!
//! Not reachable through the acalcout record — `build_inputs` resizes every array to
//! NUSE/NELM before evaluating, so the record never hands the engine an empty one —
//! but fully reachable through the public `acalc()` API, which is what these cover.
//!
//! Expected values read off the compiled upstream aCalc (gcc 13, arraySize 6) with
//! `pp_aArg[0]` passed as NULL, C's own "the record has not allocated the array".

use epics_base_rs::calc::{ArrayInputs, ArrayStackValue, acalc};

/// Nothing set: `inputs.arrays[i]` is the empty `Vec` that `ArrayInputs::new` gives.
fn unset(expr: &str) -> ArrayStackValue {
    let mut i = ArrayInputs::new(6);
    assert!(i.arrays[0].is_empty(), "AA must start unset for this test");
    acalc(expr, &mut i).unwrap()
}

fn d(expr: &str) -> f64 {
    match unset(expr) {
        ArrayStackValue::Double(v) => v,
        ArrayStackValue::Array(c) => {
            panic!("expected a SCALAR result, got the array {:?}", c.buf())
        }
    }
}

#[test]
fn an_unset_array_var_fetches_as_an_array() {
    // The shape itself. compiled C: FETCH_AA is `toArray(ps,0)` — always an array.
    match unset("AA") {
        ArrayStackValue::Array(c) => assert_eq!(c.buf(), &[0.0; 6]),
        ArrayStackValue::Double(v) => {
            panic!("an unset AA must fetch as a 6-element zero ARRAY, got the scalar {v}")
        }
    }
}

#[test]
fn index_zero_crossing_of_an_unset_array_is_minus_one() {
    // compiled C: `IXZ(AA)` with pp_aArg[0]=NULL -> -1. The ARRAY branch (`:870-883`)
    // looks for a sign CHANGE and finds none, so j stays -1.
    //
    // The port pushed the scalar 0 and took IXZ's SCALAR branch instead
    // (`|d| if d.abs() < SMALL { 0.0 }`, C `:1079` — "a scalar IS its own element 0"),
    // which answers 0.
    assert_eq!(d("IXZ(AA)"), -1.0);
}

#[test]
fn fwhm_of_an_unset_array_is_last_el() {
    // compiled C: `FWHM(AA)` with pp_aArg[0]=NULL -> 5. The ARRAY branch's
    // no-crossing fallback is `lastEl` (= arraySize-1 = 5).
    //
    // The port took FWHM's SCALAR branch (ZERO) and answered 0.
    assert_eq!(d("FWHM(AA)"), 5.0);
}

#[test]
fn the_reductions_that_agree_still_agree() {
    // Not every operator distinguishes the two — these are the controls, and they
    // must not move. compiled C, pp_aArg[0]=NULL: AVG 0, STD 0, IXNZ -1, SUM 0.
    // (`SUM` is aCalc's array-sum spelling — `aCalcPostfix.c:194` maps it to ARRSUM.
    // There is no `ARRSUM` token in either engine: it would lex as `ARR(SUM(...))`.)
    assert_eq!(d("AVG(AA)"), 0.0);
    assert_eq!(d("STD(AA)"), 0.0);
    assert_eq!(d("IXNZ(AA)"), -1.0);
    assert_eq!(d("SUM(AA)"), 0.0);
}

#[test]
fn an_unset_array_operand_still_drives_the_array_branch_of_a_binary_op() {
    // compiled C: `AA>?1` with pp_aArg[0]=NULL -> the array [1,1,1,1,1,1].
    // With a scalar 0 operand the port answered the scalar 1 instead.
    match unset("AA>?1") {
        ArrayStackValue::Array(c) => assert_eq!(c.buf(), &[1.0; 6]),
        ArrayStackValue::Double(v) => panic!("expected an array result, got the scalar {v}"),
    }
}

#[test]
fn a_set_array_var_is_unaffected() {
    let mut i = ArrayInputs::new(6);
    i.arrays[0] = vec![1.0, 2.0, 3.0, 4.0, 5.0, 6.0];
    match acalc("AA", &mut i).unwrap() {
        ArrayStackValue::Array(c) => assert_eq!(c.buf(), &[1.0, 2.0, 3.0, 4.0, 5.0, 6.0]),
        other => panic!("expected an array, got {other:?}"),
    }
}

#[test]
fn a_short_array_var_is_zero_padded_to_array_size() {
    // ArrayCell::new is the single owner of the `buf.len() == array_size` invariant,
    // and C's FETCH loop always runs `i < arraySize`.
    let mut i = ArrayInputs::new(6);
    i.arrays[0] = vec![1.0, 2.0];
    match acalc("AA", &mut i).unwrap() {
        ArrayStackValue::Array(c) => assert_eq!(c.buf(), &[1.0, 2.0, 0.0, 0.0, 0.0, 0.0]),
        other => panic!("expected an array, got {other:?}"),
    }
}
