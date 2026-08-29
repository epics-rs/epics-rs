//! R10-8 — aCalc's FIT family (`aCalcPerform.c:1005-1036`, `:1193-1285`).
//!
//! All four operators fit `y = c + b*x + a*x*x` over the operand's WINDOW with
//! `x = the element index`, and all four REPLACE the y operand with the FITTED
//! CURVE. None of them takes an x array, and none of them returns the coefficients
//! as an array — which is what the port did:
//!
//! | expression                 | C                                   | port (before)          |
//! |---------------------------|-------------------------------------|------------------------|
//! | `FITPOLY(y)`              | 1 operand, fitted curve             | demanded `FITPOLY(x,y)`|
//! | `FITMPOLY(y, mask)`       | 2 operands, window from the MASK    | demanded `(x,y,mask)`  |
//! | `FITQ(y [,c][,b][,a])`    | vararg; coefficients STORED into the| 2 operands, returned a |
//! | `FITMQ(y, mask [,c][,b][,a])` | scalar arguments the caller named| 4-element coeff array |
//!
//! Every expectation below is the output of a driver compiled from
//! `/home/stevek/work/epics-modules/calc/calcApp/src/{aCalcPerform,aCalcPostfix,calcUtil}.c`.

use epics_base_rs::calc::{ArrayInputs, ArrayStackValue, CalcError, acalc};

/// y = 1 + i + i^2 over 7 points — a quadratic the fit reproduces exactly.
fn quad7() -> ArrayInputs {
    let mut i = ArrayInputs::new(7);
    i.arrays[0] = vec![1.0, 3.0, 7.0, 13.0, 21.0, 31.0, 43.0];
    i
}

fn a(expr: &str, inputs: &mut ArrayInputs) -> Vec<f64> {
    match acalc(expr, inputs).expect("status 0") {
        ArrayStackValue::Array(cell) => cell.buf().to_vec(),
        other => panic!("expected an Array result, got {other:?}"),
    }
}

fn close(got: &[f64], want: &[f64]) {
    assert_eq!(got.len(), want.len(), "got {got:?}, want {want:?}");
    for (i, (g, w)) in got.iter().zip(want).enumerate() {
        assert!(
            (g - w).abs() < 1e-6,
            "element {i}: got {got:?}, want {want:?}"
        );
    }
}

/// `FITPOLY(y)` — ONE operand, and the result is the fitted curve. Compiled C,
/// arraySize 7, AA=[1,3,7,13,21,31,43]: aresult = AA (a perfect quadratic).
#[test]
fn r10_8_fitpoly_takes_only_y_and_returns_the_fitted_curve() {
    let mut i = quad7();
    close(
        &a("FITPOLY(AA)", &mut i),
        &[1.0, 3.0, 7.0, 13.0, 21.0, 31.0, 43.0],
    );

    // Negative control — the CURVE, not the data: a noisy y comes back smoothed onto
    // the parabola. Compiled C, AA=[0,1,10,3,4,5,6]:
    //   aresult = 0.5714285714 2.714285714 4.285714286 5.285714286 5.714285714
    //             5.571428571 4.857142857
    let mut i = ArrayInputs::new(7);
    i.arrays[0] = vec![0.0, 1.0, 10.0, 3.0, 4.0, 5.0, 6.0];
    close(
        &a("FITPOLY(AA)", &mut i),
        &[
            0.571_428_571_4,
            2.714_285_714_3,
            4.285_714_285_7,
            5.285_714_285_7,
            5.714_285_714_3,
            5.571_428_571_4,
            4.857_142_857_1,
        ],
    );
}

/// The fit runs over the WINDOW, and everything outside it is zeroed (`:1011-1012`).
/// Compiled C, AA=[0,1,10,3,4,5,6]: `FITPOLY(AA[0,3])` -> [-1.2, 4.6, 6.4, 4.2, 0,0,0].
#[test]
fn r10_8_fitpoly_fits_the_window_and_zeroes_the_rest() {
    let mut i = ArrayInputs::new(7);
    i.arrays[0] = vec![0.0, 1.0, 10.0, 3.0, 4.0, 5.0, 6.0];
    close(
        &a("FITPOLY(AA[0,3])", &mut i),
        &[-1.2, 4.6, 6.4, 4.2, 0.0, 0.0, 0.0],
    );
}

/// A scalar operand is not promoted and is not an error: FITPOLY is in C's UNARY
/// switch, whose scalar branch is `case FITPOLY: ps->d = 0;` (`:1089`). Compiled C:
/// `FITPOLY(A)` with A=5 is dresult 0, status 0; `FITPOLY(A)+2` is 2.
#[test]
fn r10_8_fitpoly_of_a_scalar_is_zero() {
    let mut i = ArrayInputs::new(5);
    i.num_vars[0] = 5.0;
    assert_eq!(
        acalc("FITPOLY(A)", &mut i).unwrap(),
        ArrayStackValue::Double(0.0)
    );
    assert_eq!(
        acalc("FITPOLY(A)+2", &mut i).unwrap(),
        ArrayStackValue::Double(2.0)
    );
}

/// Fewer than three points in the window is C's `fitpoly` -1 (`calcUtil.c:271`) with
/// no linear fallback. R10-11 (`b80ef9b3`) settled that status: the engine propagates
/// it as [`CalcError::FitFailed`], and a non-zero `status` suppresses the result write
/// entirely (`aCalcPerform.c:1602-1605`), so there is no curve to compare at this
/// boundary: the engine hands back the status and no array. The zeros a compiled C
/// driver prints for `aresult` are that driver's untouched output buffer, which is a
/// record-side observation this test cannot make.
#[test]
fn r10_8_fitpoly_under_three_points_yields_no_curve() {
    let mut i = quad7();
    assert_eq!(
        acalc("FITPOLY(AA[0,1])", &mut i),
        Err(CalcError::FitFailed),
        "a two-point window is C's fitpoly -1, not a silent zero curve"
    );
}

/// `FITMPOLY(y, mask)` — two operands, and the mask ADMITS points into the fit.
/// Compiled C, AA=[1,3,1000,13,21,31,43] (an outlier at index 2), BB=[1,1,0,1,1,1,1]:
/// aresult = [1,3,7,13,21,31,43] — the outlier is excluded and the curve is the clean
/// quadratic through the rest.
#[test]
fn r10_8_fitmpoly_mask_excludes_points_from_the_fit() {
    let mut i = ArrayInputs::new(7);
    i.arrays[0] = vec![1.0, 3.0, 1000.0, 13.0, 21.0, 31.0, 43.0];
    i.arrays[1] = vec![1.0, 1.0, 0.0, 1.0, 1.0, 1.0, 1.0];
    close(
        &a("FITMPOLY(AA,BB)", &mut i),
        &[1.0, 3.0, 7.0, 13.0, 21.0, 31.0, 43.0],
    );
}

/// The mask is a THRESHOLD — `mask[i] > SMALL`, SMALL = 1e-8 (`calcUtil.c:280`) —
/// not the `!= 0.0` the port applied. Compiled C with the same outlier:
///   mask[2] = 1e-9  -> masked OUT: aresult = [1,3,7,13,21,31,43]
///   mask[2] = 1e-7  -> kept IN:    aresult = [71.93, 215.79, ...]
#[test]
fn r10_8_fitmpoly_mask_is_a_threshold_at_1e_minus_8() {
    let mut i = ArrayInputs::new(7);
    i.arrays[0] = vec![1.0, 3.0, 1000.0, 13.0, 21.0, 31.0, 43.0];

    i.arrays[1] = vec![1.0, 1.0, 1e-9, 1.0, 1.0, 1.0, 1.0];
    close(
        &a("FITMPOLY(AA,BB)", &mut i),
        &[1.0, 3.0, 7.0, 13.0, 21.0, 31.0, 43.0],
    );

    // Negative control: 1e-7 is ABOVE the threshold, so the outlier is in the fit.
    i.arrays[1] = vec![1.0, 1.0, 1e-7, 1.0, 1.0, 1.0, 1.0];
    let got = a("FITMPOLY(AA,BB)", &mut i);
    assert!(
        (got[0] - 71.928_571_428_57).abs() < 1e-6,
        "1e-7 must keep the outlier in the fit: {got:?}"
    );
}

/// FITMPOLY's window comes from the MASK, not from y (`:1014`, `calcFirstLast` runs
/// while `ps` still points at the mask). Compiled C, AA=[1,3,7,13,21,31,43],
/// BB=all ones: `FITMPOLY(AA,BB[0,3])` -> [1,3,7,13,0,0,0] — the y operand has no
/// window of its own, yet only its first four elements survive.
#[test]
fn r10_8_fitmpoly_window_comes_from_the_mask() {
    let mut i = quad7();
    i.arrays[1] = vec![1.0; 7];
    close(
        &a("FITMPOLY(AA,BB[0,3])", &mut i),
        &[1.0, 3.0, 7.0, 13.0, 0.0, 0.0, 0.0],
    );
}

/// A SCALAR mask is a hard -1 in C: the unary dispatch tests the TOP operand, so the
/// scalar branch (`case FITMPOLY: ps->d = 0;`) runs and never DECs the y operand
/// below it; the leaked cell then trips `if (ps != top) return(-1)` (`:1608`).
/// Compiled C: `FITMPOLY(AA,1)` is status -1 with no result written.
#[test]
fn r10_8_fitmpoly_with_a_scalar_mask_fails() {
    let mut i = quad7();
    assert!(acalc("FITMPOLY(AA,1)", &mut i).is_err());
}

/// `FITQ(y, c, b, a)` — the trailing arguments NAME scalar arguments, and the fitted
/// coefficients are STORED into them. Compiled C, AA = 1+i+i^2:
///   FITQ(AA,C,D,E) -> aresult = AA (the fitted curve), C=1, D=1, E=1
/// i.e. the FIRST named argument takes the CONSTANT term, the second the linear one,
/// the third the quadratic one.
#[test]
fn r10_8_fitq_stores_the_coefficients_into_the_named_arguments() {
    let mut i = quad7();
    close(
        &a("FITQ(AA,C,D,E)", &mut i),
        &[1.0, 3.0, 7.0, 13.0, 21.0, 31.0, 43.0],
    );
    assert!((i.num_vars[2] - 1.0).abs() < 1e-6, "C = constant term");
    assert!((i.num_vars[3] - 1.0).abs() < 1e-6, "D = linear term");
    assert!((i.num_vars[4] - 1.0).abs() < 1e-6, "E = quadratic term");
}

/// The coefficient arguments are OPTIONAL, and a shorter list fills the terms from
/// the constant up. `FITQ(AA)` fits and stores nothing; `FITQ(AA,C)` stores only the
/// constant term. (C reads the targets from the stack top down: `case 4: argc; case 3:
/// argb; case 2: arga;` — `:1199-1211`.)
#[test]
fn r10_8_fitq_coefficient_arguments_are_optional() {
    let mut i = quad7();
    i.num_vars[2] = -99.0;
    i.num_vars[3] = -99.0;
    close(
        &a("FITQ(AA)", &mut i),
        &[1.0, 3.0, 7.0, 13.0, 21.0, 31.0, 43.0],
    );
    assert_eq!(i.num_vars[2], -99.0, "FITQ(AA) names no target");
    assert_eq!(i.num_vars[3], -99.0);

    close(
        &a("FITQ(AA,C)", &mut i),
        &[1.0, 3.0, 7.0, 13.0, 21.0, 31.0, 43.0],
    );
    assert!(
        (i.num_vars[2] - 1.0).abs() < 1e-6,
        "the lone target is the CONSTANT"
    );
    assert_eq!(i.num_vars[3], -99.0, "D is not a target here");
}

/// Arguments past the fourth are discarded from the TOP (`while (nargs>4) DEC(ps)`,
/// `:1198`), so an over-long call loses its LAST argument, not its first. Compiled C:
/// `FITQ(AA,C,D,E,F)` -> C=1, D=1, E=1 and F untouched.
#[test]
fn r10_8_fitq_discards_extra_arguments_from_the_top() {
    let mut i = quad7();
    i.num_vars[5] = -99.0;
    close(
        &a("FITQ(AA,C,D,E,F)", &mut i),
        &[1.0, 3.0, 7.0, 13.0, 21.0, 31.0, 43.0],
    );
    assert!((i.num_vars[2] - 1.0).abs() < 1e-6, "C = constant term");
    assert!((i.num_vars[3] - 1.0).abs() < 1e-6, "D = linear term");
    assert!((i.num_vars[4] - 1.0).abs() < 1e-6, "E = quadratic term");
    assert_eq!(i.num_vars[5], -99.0, "the FIFTH argument is discarded");
}

/// `FITMQ(y, mask, c, b, a)` — FITQ with a mask, and unlike FITMPOLY it really does
/// `toArray` its mask (`:1261`), so a scalar mask is legal. Compiled C, with the
/// outlier masked out: aresult = the clean curve, C=1, D=1, E=1.
#[test]
fn r10_8_fitmq_masks_and_stores() {
    let mut i = ArrayInputs::new(7);
    i.arrays[0] = vec![1.0, 3.0, 1000.0, 13.0, 21.0, 31.0, 43.0];
    i.arrays[1] = vec![1.0, 1.0, 0.0, 1.0, 1.0, 1.0, 1.0];
    close(
        &a("FITMQ(AA,BB,C,D,E)", &mut i),
        &[1.0, 3.0, 7.0, 13.0, 21.0, 31.0, 43.0],
    );
    assert!((i.num_vars[2] - 1.0).abs() < 1e-6);
    assert!((i.num_vars[3] - 1.0).abs() < 1e-6);
    assert!((i.num_vars[4] - 1.0).abs() < 1e-6);

    // A scalar mask of 1 admits every point (1 > SMALL), so the outlier is back in.
    let got = a("FITMQ(AA,1,C,D,E)", &mut i);
    assert!(
        (got[0] - 1.0).abs() > 1e-6,
        "a scalar-1 mask admits the outlier: {got:?}"
    );
}

/// C `:1257-1260` — FITMQ with fewer than two arguments is an immediate `return(-1)`,
/// not a deferred status. Compiled C: `FITMQ(AA)` prints "need at least two arguments"
/// and answers -1.
#[test]
fn r10_8_fitmq_needs_at_least_two_arguments() {
    let mut i = quad7();
    assert!(acalc("FITMQ(AA)", &mut i).is_err());
}
