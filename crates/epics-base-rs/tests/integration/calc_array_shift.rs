//! R8-8 — in aCalc, `<<` and `>>` are ONE C arm whose meaning is chosen by the
//! LEFT operand's type (`aCalcPerform.c:1416-1459`):
//!
//! * scalar left  -> a bit shift by the `(int)` count (`:1410-1416`);
//! * array left   -> a POSITIONAL move of the elements by `myNINT(count)`
//!   (`<<` negates the count), zero-filling the vacated end, and — because the
//!   count is a double — a LINEAR INTERPOLATION of its fractional part
//!   (`:1428-1458`). Nothing bitwise happens to an array.
//!
//! The port bit-shifted element-wise in both cases, so `AA>>0.5` truncated the
//! count to 0 and `AA>>1` returned `[0,1,1,2,2,3,3,4]` instead of a shifted
//! array.
//!
//! Every expected array below is a line printed by `aCalcPostfix.c` +
//! `aCalcPerform.c` built standalone out of `epics-modules/calc/calcApp/src`
//! and driven with these expressions and inputs.

use epics_base_rs::calc::{ArrayInputs, ArrayStackValue, acalc};

/// AA = [1..8], BB = [2,0,0,0,0,0,0,0] — the C driver's inputs.
fn run(expr: &str) -> ArrayStackValue {
    let mut inp = ArrayInputs::new(8);
    inp.arrays[0] = vec![1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0, 8.0];
    inp.arrays[1] = vec![2.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0];
    acalc(expr, &mut inp).expect("aCalcPerform returns st=0 here")
}

fn arr(expr: &str) -> Vec<f64> {
    match run(expr) {
        ArrayStackValue::Array(v) => v.into_buf(),
        ArrayStackValue::Double(d) => panic!("{expr}: expected an array result, got {d}"),
    }
}

fn scalar(expr: &str) -> f64 {
    match run(expr) {
        ArrayStackValue::Double(d) => d,
        ArrayStackValue::Array(v) => panic!("{expr}: expected a scalar result, got {v:?}"),
    }
}

/// C: `AA>>1` -> [0,1,2,3,4,5,6,7]; `AA<<1` -> [2,3,4,5,6,7,8,0].
/// An integral count is a pure positional move with a zero fill.
#[test]
fn array_shift_by_whole_count_moves_elements() {
    assert_eq!(arr("AA>>1"), vec![0.0, 1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0]);
    assert_eq!(arr("AA<<1"), vec![2.0, 3.0, 4.0, 5.0, 6.0, 7.0, 8.0, 0.0]);
    assert_eq!(arr("AA>>2"), vec![0.0, 0.0, 1.0, 2.0, 3.0, 4.0, 5.0, 6.0]);
    // C: a negative right shift is a left shift (`AA>>-1` == `AA<<1`).
    assert_eq!(arr("AA>>-1"), vec![2.0, 3.0, 4.0, 5.0, 6.0, 7.0, 8.0, 0.0]);
}

/// C: `AA<<9` and `AA>>8` -> all zeros. A count at or past the array length
/// empties it; C's move loops simply do not execute and the zero fill covers
/// everything.
#[test]
fn array_shift_past_the_end_zeroes_the_array() {
    assert_eq!(arr("AA<<9"), vec![0.0; 8]);
    assert_eq!(arr("AA>>8"), vec![0.0; 8]);
}

/// The fractional part of the count is LINEARLY INTERPOLATED (`:1429-1446`),
/// in place and in C's walk order — the extrapolated end point reads the
/// neighbour the same pass has already overwritten.
///
/// C: `AA>>0.5`  -> [0.5,1.5,2.5,3.5,4.5,5.5,6.5,7.25]
/// C: `AA<<0.5`  -> [1.75,2.5,3.5,4.5,5.5,6.5,7.5,4]
/// C: `AA>>1.25` -> [-0.1875,0.75,1.75,2.75,3.75,4.75,5.75,6.75]
#[test]
fn array_shift_interpolates_the_fractional_count() {
    assert_eq!(
        arr("AA>>0.5"),
        vec![0.5, 1.5, 2.5, 3.5, 4.5, 5.5, 6.5, 7.25]
    );
    assert_eq!(
        arr("AA<<0.5"),
        vec![1.75, 2.5, 3.5, 4.5, 5.5, 6.5, 7.5, 4.0]
    );
    assert_eq!(
        arr("AA>>1.25"),
        vec![-0.1875, 0.75, 1.75, 2.75, 3.75, 4.75, 5.75, 6.75]
    );
}

/// C `:1409` collapses an array count with `to_double` — its `a[0]`.
/// C: `AA>>BB` with BB=[2,0,...] -> the same as `AA>>2`.
#[test]
fn array_shift_count_is_the_first_element_of_an_array_count() {
    assert_eq!(arr("AA>>BB"), vec![0.0, 0.0, 1.0, 2.0, 3.0, 4.0, 5.0, 6.0]);
}

/// A SCALAR left operand keeps the bit shift (`:1410-1416`), including when the
/// count is an array (collapsed to `a[0]` = 1 here).
/// C: `6>>1` -> 3, `6<<1` -> 12, `2<<AA` (AA[0]=1) -> 4.
#[test]
fn scalar_left_operand_is_still_a_bit_shift() {
    assert_eq!(scalar("6>>1"), 3.0);
    assert_eq!(scalar("6<<1"), 12.0);
    assert_eq!(scalar("2<<AA"), 4.0); // AA[0] = 1
    assert_eq!(scalar("2>>AA"), 1.0); // 2 >> 1
}
