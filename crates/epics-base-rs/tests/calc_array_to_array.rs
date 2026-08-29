//! R11-6 — `ARR(x)` is C's `toArray(ps,1)` (`aCalcPerform.c:1516-1518`), the same
//! promotion every other operator uses, applied IN PLACE.
//!
//! The port popped the operand as an `f64` and rebuilt a buffer from it, which broke
//! the operator at both ends:
//!
//! * an ARRAY operand was collapsed to its `a[0]` and rebroadcast — C leaves it alone,
//!   window and all, because `toArray` tests `isDouble` first (`:146`).
//! * a NaN scalar was copied verbatim — C's `to_array` fills 0 for a NaN
//!   (`:134-136`), so an aCalcout could not publish an all-NaN AVAL.
//!
//! Every expectation below is the output of a driver compiled from
//! `/home/stevek/work/epics-modules/calc/calcApp/src/{aCalcPerform,aCalcPostfix,calcUtil}.c`.

use epics_base_rs::calc::{ArrayInputs, ArrayStackValue, acalc};

fn inputs() -> ArrayInputs {
    let mut i = ArrayInputs::new(4);
    i.arrays[0] = vec![1.0, 2.0, 3.0, 4.0];
    i
}

fn buf(expr: &str, inputs: &mut ArrayInputs) -> Vec<f64> {
    match acalc(expr, inputs).expect("status 0") {
        ArrayStackValue::Array(cell) => cell.buf().to_vec(),
        other => panic!("ARR always yields an array, got {other:?}"),
    }
}

/// C's `to_array` fills 0 for a NaN scalar (`:134-136`) — the ONE place aCalc keeps a
/// NaN out of an array. Compiled C, arraySize 4, A=NaN: `ARR(A)` is [0,0,0,0].
#[test]
fn r11_6_arr_of_a_nan_scalar_fills_zero() {
    let mut i = inputs();
    i.num_vars[0] = f64::NAN;
    assert_eq!(buf("ARR(A)", &mut i), vec![0.0, 0.0, 0.0, 0.0]);
}

/// The negative control: an ordinary scalar is broadcast, not zeroed. Compiled C, A=5:
/// `ARR(A)` is [5,5,5,5].
#[test]
fn r11_6_arr_of_an_ordinary_scalar_broadcasts_it() {
    let mut i = inputs();
    i.num_vars[0] = 5.0;
    assert_eq!(buf("ARR(A)", &mut i), vec![5.0, 5.0, 5.0, 5.0]);
}

/// An ARRAY operand passes straight through — `toArray` is a no-op on it. Compiled C,
/// AA=[1,2,3,4]: `ARR(AA)` is [1,2,3,4]. The port answered [1,1,1,1].
#[test]
fn r11_6_arr_of_an_array_is_a_no_op() {
    assert_eq!(buf("ARR(AA)", &mut inputs()), vec![1.0, 2.0, 3.0, 4.0]);
}

/// And the window survives it, because `toArray` never reaches an array's numEl.
/// Compiled C: `ARR(AA[1,2])` is [2,3,0,0] and `AVG(ARR(AA[1,2]))` is 2.5 — the same
/// answer as `AVG(AA[1,2])`, which is the point: ARR must not flatten the window into
/// the buffer's zero tail (that would average to 1.25).
#[test]
fn r11_6_arr_preserves_the_window() {
    assert_eq!(buf("ARR(AA[1,2])", &mut inputs()), vec![2.0, 3.0, 0.0, 0.0]);
    assert_eq!(
        acalc("AVG(ARR(AA[1,2]))", &mut inputs()).unwrap(),
        ArrayStackValue::Double(2.5)
    );
}
