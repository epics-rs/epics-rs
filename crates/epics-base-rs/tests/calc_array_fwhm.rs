//! R11-11 — aCalc's FWHM (`aCalcPerform.c:928-967`): a STRICT half-max test, and
//! no-crossing fallbacks of `lastEl` (forward) and `0` (backward).
//!
//! ```c
//! d = e + (d-e)/2;                                  /* half = min + (max-min)/2 */
//! for (i=j+1, found=0; i<=lastEl; i++) {
//!     if (ps->a[i] < d) { found=1; e = (i-1) + (d - a[i-1])/(a[i] - a[i-1]); break; }
//! }
//! if (!found) e = lastEl;
//! for (i=j-1, found=0; i>=firstEl; i--) {
//!     if (ps->a[i] < d) { found=1; d = i + (d - a[i])/(a[i+1] - a[i]); break; }
//! }
//! if (!found) d = 0;
//! ps->d = e-d;
//! ```
//!
//! The port used `<=` and initialised BOTH crossings to the peak index, so a side
//! with no crossing contributed 0 width instead of running to the end of the window.
//!
//! Every expectation below is the output of a driver compiled from
//! `/home/stevek/work/epics-modules/calc/calcApp/src/{aCalcPerform,aCalcPostfix,calcUtil}.c`.

use epics_base_rs::calc::{ArrayInputs, ArrayStackValue, acalc};

fn d(expr: &str, arr: Vec<f64>) -> f64 {
    let mut i = ArrayInputs::new(arr.len());
    i.arrays[0] = arr;
    match acalc(expr, &mut i).expect("status 0") {
        ArrayStackValue::Double(v) => v,
        other => panic!("expected a Double result, got {other:?}"),
    }
}

/// The ordinary case, unchanged by this fix — the negative control for everything
/// below. Compiled C, AA=[0,1,4,1,0]: 1.3333333333333333.
#[test]
fn r11_11_a_symmetric_peak_is_measured_between_its_crossings() {
    assert!((d("FWHM(AA)", vec![0.0, 1.0, 4.0, 1.0, 0.0]) - 4.0 / 3.0).abs() < 1e-12);
}

/// The test is STRICT: a sample sitting exactly AT half-max is not a crossing, so the
/// walk steps over a half-max plateau. Compiled C, AA=[0,2,4,2,2,0] (half = 2): 3.
/// The port's `<=` stopped on the first plateau sample and answered 2.
#[test]
fn r11_11_a_sample_exactly_at_half_max_is_not_a_crossing() {
    assert_eq!(d("FWHM(AA)", vec![0.0, 2.0, 4.0, 2.0, 2.0, 0.0]), 3.0);
}

/// A monotonic ramp peaks at its LAST element, so there is no forward crossing and C
/// measures to the end of the window: `e = lastEl` (`:944`). Compiled C,
/// AA=[0,1,2,3,4]: 2 — the port answered ~0, because it left the forward crossing at
/// the peak index.
#[test]
fn r11_11_no_forward_crossing_measures_to_the_last_element() {
    assert_eq!(d("FWHM(AA)", vec![0.0, 1.0, 2.0, 3.0, 4.0]), 2.0);
}

/// Mirror image: a descending ramp peaks at element 0, so there is no backward
/// crossing and C measures from the START, `d = 0` (`:955`). Compiled C,
/// AA=[4,3,2,1,0]: 2.
#[test]
fn r11_11_no_backward_crossing_measures_from_the_first_element() {
    assert_eq!(d("FWHM(AA)", vec![4.0, 3.0, 2.0, 1.0, 0.0]), 2.0);
}

/// Both fallbacks at once. A flat array has max == min, so half-max IS the value and
/// nothing is strictly below it: C answers `lastEl - 0`. Compiled C, AA=[3,3,3,3,3]:
/// 4. The port's `max == min` early return answered 0.
#[test]
fn r11_11_a_flat_array_is_the_full_width() {
    assert_eq!(d("FWHM(AA)", vec![3.0, 3.0, 3.0, 3.0, 3.0]), 4.0);
}

/// FWHM reads the operand's WINDOW, and its fallbacks are the window's bounds.
/// Compiled C, arraySize 6, AA=[0,1,4,1,0,9]: `FWHM(AA[1,4])` = 1.3333333333333333 —
/// the peak's neighbours, with the out-of-window 9 excluded.
#[test]
fn r11_11_fwhm_is_measured_over_the_window() {
    let mut i = ArrayInputs::new(6);
    i.arrays[0] = vec![0.0, 1.0, 4.0, 1.0, 0.0, 9.0];
    let got = match acalc("FWHM(AA[1,4])", &mut i).expect("status 0") {
        ArrayStackValue::Double(v) => v,
        other => panic!("expected a Double result, got {other:?}"),
    };
    assert!((got - 4.0 / 3.0).abs() < 1e-12, "got {got}");
}

/// The `lastEl` fallback is literally `lastEl`, so an EMPTY window (numEl 0, lastEl
/// -1) answers -1 — C still seeds from `a[firstEl]` (in bounds) and neither walk
/// runs. Compiled C: `FWHM(AA[2,1])` = -1.
#[test]
fn r11_11_an_empty_window_is_minus_one() {
    let mut i = ArrayInputs::new(5);
    i.arrays[0] = vec![0.0, 1.0, 4.0, 1.0, 0.0];
    let got = match acalc("FWHM(AA[2,1])", &mut i).expect("status 0") {
        ArrayStackValue::Double(v) => v,
        other => panic!("expected a Double result, got {other:?}"),
    };
    assert_eq!(got, -1.0, "lastEl is -1 and neither crossing is found");
}

/// A scalar operand is C's `case FWHM: ps->d = 0;` (`:1086`), not an error.
#[test]
fn r11_11_fwhm_of_a_scalar_is_zero() {
    let mut i = ArrayInputs::new(5);
    i.num_vars[0] = 7.0;
    assert_eq!(
        acalc("FWHM(A)", &mut i).unwrap(),
        ArrayStackValue::Double(0.0)
    );
}
